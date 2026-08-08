//! Discover and remove untracked worktree content without host assumptions.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::worktree::worktree_path as clean_worktree_path;
use crate::{Error, IgnoreMatcher, Repository, Result};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CleanIgnoredMode {
    /// Preserve paths ignored by repository ignore rules.
    #[default]
    Respect,
    /// Ignore standard rules and remove ignored plus non-ignored paths (`-x`).
    Include,
    /// Remove only ignored paths (`-X`).
    Only,
}

/// Selection and destructive-authority policy for cleaning a worktree.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanOptions {
    /// Permit removal of wholly untracked directories.
    pub directories: bool,
    pub ignored: CleanIgnoredMode,
    /// Additional ignore patterns, corresponding to repeated `-e` arguments.
    pub exclude_patterns: Vec<Vec<u8>>,
    /// Required for mutation. Dry-run discovery does not require force.
    pub force: bool,
    /// Permit removal of untracked directories containing `.git`.
    pub remove_nested_repositories: bool,
    pub dry_run: bool,
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self {
            directories: false,
            ignored: CleanIgnoredMode::Respect,
            exclude_patterns: Vec::new(),
            force: false,
            remove_nested_repositories: false,
            dry_run: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanEntry {
    path: Vec<u8>,
    directory: bool,
}

impl CleanEntry {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.directory
    }
}

impl Repository {
    /// Discover and optionally remove untracked content below literal paths.
    ///
    /// Empty `paths` selects the whole worktree. Discovery finishes before the
    /// first mutation and returns stable repository-relative paths. Mutating
    /// calls require `force`; nested repositories require the separate
    /// `remove_nested_repositories` authority.
    ///
    /// # Errors
    /// Returns an error for a bare repository, unsafe path, missing force,
    /// malformed ignores, inaccessible storage, or deletion failure.
    pub fn clean<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: &CleanOptions,
    ) -> Result<Vec<CleanEntry>> {
        if !options.dry_run && !options.force {
            return Err(Error::InvalidRepository(
                "clean mutation requires force=true".into(),
            ));
        }
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("clean requires a non-bare repository".into())
        })?;
        let requested = paths
            .iter()
            .map(|path| normalize_clean_path(path.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let index = self.read_index()?;
        let tracked = index
            .entries()
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let gitlinks = index
            .entries()
            .iter()
            .filter(|entry| entry.mode() == 0o160_000)
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let mut ignores = self.ignore_matcher()?;
        let mut manual = IgnoreMatcher::default();
        for pattern in &options.exclude_patterns {
            manual.add_patterns(b"", pattern)?;
        }
        let mut entries = Vec::new();
        let context = CleanContext {
            root: work_tree,
            requested: &requested,
            tracked: &tracked,
            gitlinks: &gitlinks,
            manual: &manual,
            options,
        };
        self.collect_clean_entries(Path::new(""), &mut ignores, &context, &mut entries)?;
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        if options.dry_run {
            return Ok(entries);
        }
        for entry in &entries {
            let full = work_tree.join(clean_worktree_path(&entry.path)?);
            if entry.directory {
                remove_clean_tree(self, &full)?;
            } else {
                self.filesystem().remove_file(&full)?;
                prune_clean_parents(self, work_tree, clean_worktree_path(&entry.path)?.parent())?;
            }
        }
        Ok(entries)
    }

    fn collect_clean_entries(
        &self,
        relative: &Path,
        ignores: &mut IgnoreMatcher,
        context: &CleanContext<'_>,
        output: &mut Vec<CleanEntry>,
    ) -> Result<()> {
        let relative_bytes = clean_index_path(relative)?;
        ignores.add_worktree_patterns(self, context.root, &relative_bytes)?;
        let directory = context.root.join(relative);
        for child in self.filesystem().read_dir(&directory)? {
            let child_relative = relative.join(child);
            if child_relative == Path::new(".git")
                || context.root.join(&child_relative) == self.git_dir()
            {
                continue;
            }
            let path = clean_index_path(&child_relative)?;
            if context.gitlinks.contains(&path) {
                continue;
            }
            let metadata = self
                .filesystem()
                .metadata(&context.root.join(&child_relative))?;
            let selected = clean_selected(&path, context.requested);
            let ancestor = clean_selection_below(&path, context.requested);
            if metadata.is_dir() {
                let tracked_below = clean_tracked_below(context.tracked, &path);
                if tracked_below {
                    self.collect_clean_entries(&child_relative, ignores, context, output)?;
                    continue;
                }
                if !selected && !ancestor {
                    continue;
                }
                if nested_repository(self, context.root, &child_relative)?
                    && !context.options.remove_nested_repositories
                {
                    continue;
                }
                let repository_ignored = ignores.is_ignored(&path, true);
                let manual = context.manual.is_ignored(&path, true);
                let removable =
                    clean_mode_matches(context.options.ignored, repository_ignored, manual);
                let fully_selected = selected && !ancestor;
                if context.options.directories && fully_selected && removable {
                    output.push(CleanEntry {
                        path,
                        directory: true,
                    });
                } else if ancestor
                    || (context.options.ignored == CleanIgnoredMode::Only
                        && !repository_ignored
                        && !manual)
                {
                    self.collect_clean_entries(&child_relative, ignores, context, output)?;
                }
            } else if selected && !context.tracked.contains(&path) {
                let repository_ignored = ignores.is_ignored(&path, false);
                let manual = context.manual.is_ignored(&path, false);
                if clean_mode_matches(context.options.ignored, repository_ignored, manual) {
                    output.push(CleanEntry {
                        path,
                        directory: false,
                    });
                }
            }
        }
        Ok(())
    }
}

struct CleanContext<'a> {
    root: &'a Path,
    requested: &'a [Vec<u8>],
    tracked: &'a BTreeSet<Vec<u8>>,
    gitlinks: &'a BTreeSet<Vec<u8>>,
    manual: &'a IgnoreMatcher,
    options: &'a CleanOptions,
}

fn clean_mode_matches(mode: CleanIgnoredMode, ignored: bool, manual: bool) -> bool {
    match mode {
        CleanIgnoredMode::Respect => !ignored && !manual,
        CleanIgnoredMode::Include => !manual,
        CleanIgnoredMode::Only => ignored || manual,
    }
}

fn clean_selected(path: &[u8], requested: &[Vec<u8>]) -> bool {
    requested.is_empty()
        || requested.iter().any(|prefix| {
            prefix.is_empty()
                || path == prefix
                || (path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/'))
        })
}

fn clean_selection_below(path: &[u8], requested: &[Vec<u8>]) -> bool {
    requested
        .iter()
        .any(|selection| selection.starts_with(path) && selection.get(path.len()) == Some(&b'/'))
}

fn clean_tracked_below(tracked: &BTreeSet<Vec<u8>>, path: &[u8]) -> bool {
    let mut prefix = path.to_vec();
    prefix.push(b'/');
    tracked
        .range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

fn nested_repository(repository: &Repository, root: &Path, relative: &Path) -> Result<bool> {
    repository
        .filesystem()
        .exists(&root.join(relative).join(".git"))
}

fn remove_clean_tree(repository: &Repository, path: &Path) -> Result<()> {
    for child in repository.filesystem().read_dir(path)? {
        let child = path.join(child);
        if repository.filesystem().metadata(&child)?.is_dir() {
            remove_clean_tree(repository, &child)?;
        } else {
            repository.filesystem().remove_file(&child)?;
        }
    }
    repository.filesystem().remove_dir(path)
}

fn prune_clean_parents(
    repository: &Repository,
    root: &Path,
    mut parent: Option<&Path>,
) -> Result<()> {
    while let Some(relative) = parent {
        if relative.as_os_str().is_empty() {
            break;
        }
        match repository.filesystem().remove_dir(&root.join(relative)) {
            Ok(()) | Err(Error::NotFound(_) | Error::DirectoryNotEmpty(_)) => {}
            Err(error) => return Err(error),
        }
        parent = relative.parent();
    }
    Ok(())
}

fn normalize_clean_path(path: &Path) -> Result<Vec<u8>> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvalidPath(path.to_path_buf()));
            }
        }
    }
    clean_index_path(&normalized)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn clean_index_path(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let mut output = Vec::new();
    for component in path.components() {
        if let Component::Normal(value) = component {
            if !output.is_empty() {
                output.push(b'/');
            }
            output.extend_from_slice(value.as_bytes());
        }
    }
    Ok(output)
}

#[cfg(not(unix))]
fn clean_index_path(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|path| path.replace('\\', "/").into_bytes())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, FileSystem, InitOptions, MemoryFileSystem, Signature};

    fn fixture() -> (Repository, MemoryFileSystem) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .create_dir_all(Path::new("repo/tracked-dir"))
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked"), b"tracked")
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked-dir/file"), b"tracked")
            .unwrap();
        filesystem
            .write(Path::new("repo/.gitignore"), b"*.log\nignored-dir/\n")
            .unwrap();
        repository.add(".").unwrap();
        let signature = Signature::new("Clean", "clean@example.com", 100, 0).unwrap();
        repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();

        filesystem
            .write(Path::new("repo/root.tmp"), b"tmp")
            .unwrap();
        filesystem
            .write(Path::new("repo/root.log"), b"log")
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked-dir/extra"), b"extra")
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/untracked-dir"))
            .unwrap();
        filesystem
            .write(Path::new("repo/untracked-dir/file"), b"file")
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/ignored-dir"))
            .unwrap();
        filesystem
            .write(Path::new("repo/ignored-dir/file"), b"ignored")
            .unwrap();
        filesystem
            .write(Path::new("repo/manual.keep"), b"manual")
            .unwrap();
        (repository, filesystem)
    }

    fn paths(entries: &[CleanEntry]) -> Vec<Vec<u8>> {
        entries.iter().map(|entry| entry.path.clone()).collect()
    }

    #[test]
    fn default_discovers_files_in_tracked_dirs_but_not_wholly_untracked_dirs() {
        let (repository, _) = fixture();
        let entries = repository
            .clean::<&str>(&[], &CleanOptions::default())
            .unwrap();
        assert_eq!(
            paths(&entries),
            vec![
                b"manual.keep".to_vec(),
                b"root.tmp".to_vec(),
                b"tracked-dir/extra".to_vec(),
            ]
        );
        assert!(entries.iter().all(|entry| !entry.is_directory()));
    }

    #[test]
    fn directory_and_ignored_modes_match_git_selection_layers() {
        let (repository, _) = fixture();
        let entries = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            paths(&entries),
            vec![
                b"manual.keep".to_vec(),
                b"root.tmp".to_vec(),
                b"tracked-dir/extra".to_vec(),
                b"untracked-dir".to_vec(),
            ]
        );
        assert!(entries.last().unwrap().is_directory());

        let entries = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ignored: CleanIgnoredMode::Include,
                    exclude_patterns: vec![b"manual.keep".to_vec()],
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            paths(&entries),
            vec![
                b"ignored-dir".to_vec(),
                b"root.log".to_vec(),
                b"root.tmp".to_vec(),
                b"tracked-dir/extra".to_vec(),
                b"untracked-dir".to_vec(),
            ]
        );

        let entries = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ignored: CleanIgnoredMode::Only,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            paths(&entries),
            vec![b"ignored-dir".to_vec(), b"root.log".to_vec()]
        );
    }

    #[test]
    fn explicit_descendant_is_cleanable_without_directory_mode() {
        let (repository, _) = fixture();
        let entries = repository
            .clean(&["untracked-dir/file"], &CleanOptions::default())
            .unwrap();
        assert_eq!(paths(&entries), vec![b"untracked-dir/file".to_vec()]);
    }

    #[test]
    fn mutation_requires_force_and_removes_only_preflighted_entries() {
        let (repository, filesystem) = fixture();
        assert!(
            repository
                .clean::<&str>(
                    &[],
                    &CleanOptions {
                        dry_run: false,
                        ..CleanOptions::default()
                    }
                )
                .is_err()
        );
        let removed = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    force: true,
                    dry_run: false,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert_eq!(removed.len(), 4);
        assert!(!filesystem.exists(Path::new("repo/root.tmp")).unwrap());
        assert!(!filesystem.exists(Path::new("repo/untracked-dir")).unwrap());
        assert!(filesystem.exists(Path::new("repo/root.log")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/ignored-dir/file"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/tracked")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/file"))
                .unwrap()
        );
    }

    #[test]
    fn nested_repository_needs_separate_removal_authority() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/nested/.git"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/nested/.git/HEAD"),
                b"ref: refs/heads/main\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/nested/file"), b"nested")
            .unwrap();
        let protected = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(!paths(&protected).contains(&b"nested".to_vec()));
        let selected = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    remove_nested_repositories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&selected).contains(&b"nested".to_vec()));
        repository
            .clean(
                &["nested"],
                &CleanOptions {
                    directories: true,
                    force: true,
                    remove_nested_repositories: true,
                    dry_run: false,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(!filesystem.exists(Path::new("repo/nested")).unwrap());
    }
}
