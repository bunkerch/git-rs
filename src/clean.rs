//! Discover and remove untracked worktree content without host assumptions.

use std::collections::BTreeSet;
use std::ffi::OsStr;
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
    /// Like `git clean`, a directory with tracked descendants is recursed into
    /// even when it contains a nested repository (only its untracked content is
    /// removed), and a real nested repository's `.git` — a directory or
    /// symlink validated like git's `is_nonbare_repository_dir`, or a `gitdir:`
    /// gitfile pointing at a real git directory, at any depth — is never
    /// removed without that authority. Plain, HEAD-only, or garbage entries
    /// merely named `.git` are cleaned, exactly as git cleans them. With
    /// `remove_nested_repositories`, nested repositories and their emptied
    /// ancestors are removed wholesale
    /// (mirroring `git clean -ffd`).
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
            let worktree_path = clean_worktree_path(&entry.path)?;
            let parent = worktree_path.parent();
            if entry.directory {
                remove_clean_tree(self, &full)?;
                prune_clean_parents(self, work_tree, parent)?;
            } else {
                self.filesystem().remove_file(&full)?;
                prune_clean_parents(self, work_tree, parent)?;
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
    ) -> Result<bool> {
        let relative_bytes = clean_index_path(relative)?;
        ignores.add_worktree_patterns(self, context.root, &relative_bytes)?;
        let directory = context.root.join(relative);
        let mut preserved = false;
        for child in self.filesystem().read_dir(&directory)? {
            let child_relative = relative.join(child);
            if child_relative.file_name() == Some(OsStr::new(".git"))
                || context.root.join(&child_relative) == self.git_dir()
            {
                // Skip `.git` at every traversal level, mirroring `treat_path`
                // (dir.c:2439). git matches the exact entry name: `.gitignore`
                // and other `.git*` paths are cleaned normally. `fspathcmp` is
                // case-insensitive only on case-insensitive filesystems
                // (`ignore_case`), so the exact match preserves that default.
                continue;
            }
            let path = clean_index_path(&child_relative)?;
            if context.gitlinks.contains(&path) {
                preserved = true;
                continue;
            }
            let selected = clean_selected(&path, context.requested);
            let ancestor = clean_selection_below(&path, context.requested);
            let metadata = match self.filesystem().metadata(&context.root.join(&child_relative)) {
                Ok(metadata) => metadata,
                Err(Error::NotFound(_)) => continue,
                Err(_) => {
                    // A special file (FIFO/socket) cannot be stat'd; treat it
                    // as a removable non-directory rather than aborting the
                    // traversal.
                    if selected && !context.tracked.contains(&path) {
                        let repository_ignored = ignores.is_ignored(&path, false);
                        let manual = context.manual.is_ignored(&path, false);
                        if clean_mode_matches(context.options.ignored, repository_ignored, manual) {
                            output.push(CleanEntry {
                                path,
                                directory: false,
                            });
                        } else {
                            preserved = true;
                        }
                    } else {
                        preserved = true;
                    }
                    continue;
                }
            };
            if metadata.is_dir() {
                let tracked_below = clean_tracked_below(context.tracked, &path);
                if tracked_below {
                    preserved = true;
                    self.collect_clean_entries(&child_relative, ignores, context, output)?;
                    continue;
                }
                if !selected && !ancestor {
                    preserved = true;
                    continue;
                }
                let nested = nested_repository(self, context.root, &child_relative);
                if nested && !context.options.remove_nested_repositories {
                    preserved = true;
                    continue;
                }
                let repository_ignored = ignores.is_ignored(&path, true);
                let manual = context.manual.is_ignored(&path, true);
                let removable =
                    clean_mode_matches(context.options.ignored, repository_ignored, manual);
                let fully_selected = selected && !ancestor;
                if context.options.directories && fully_selected && removable {
                    let mut temp = Vec::new();
                    let child_preserved =
                        self.collect_clean_entries(&child_relative, ignores, context, &mut temp)?;
                    if child_preserved {
                        preserved = true;
                        output.extend(temp);
                    } else {
                        output.push(CleanEntry {
                            path,
                            directory: true,
                        });
                    }
                } else if ancestor
                    || (context.options.ignored == CleanIgnoredMode::Only
                        && !repository_ignored
                        && !manual)
                {
                    let child_preserved =
                        self.collect_clean_entries(&child_relative, ignores, context, output)?;
                    preserved |= child_preserved;
                } else {
                    preserved = true;
                }
            } else if selected && !context.tracked.contains(&path) {
                let repository_ignored = ignores.is_ignored(&path, false);
                let manual = context.manual.is_ignored(&path, false);
                if clean_mode_matches(context.options.ignored, repository_ignored, manual) {
                    output.push(CleanEntry {
                        path,
                        directory: false,
                    });
                } else {
                    preserved = true;
                }
            } else {
                preserved = true;
            }
        }
        Ok(preserved)
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

fn nested_repository(repository: &Repository, root: &Path, relative: &Path) -> bool {
    clean_git_repository(repository, &root.join(relative).join(".git"))
}

/// True when `entry` (a path named `.git`) is a real repository, matching
/// git's `is_nonbare_repository_dir` (setup.c): a git directory with a valid
/// `HEAD` **and** `objects` **and** `refs` (directly or via a `commondir`
/// linked worktree), a symlink whose target is such a directory, or a
/// `gitdir: <path>` gitfile whose target is such a directory. Plain or
/// HEAD-only directories and garbage files named `.git` are not repositories
/// and are cleaned, exactly as git cleans them. Probing errors (e.g. a FIFO or
/// socket named `.git`) are treated as not-a-repository so the traversal is
/// not aborted; an unreadable `.git` file is conservatively preserved, as git
/// does on open/read failure. Symlink and gitfile targets are followed up to
/// 16 hops.
fn clean_git_repository(repository: &Repository, entry: &Path) -> bool {
    clean_git_repository_depth(repository, entry, 16).unwrap_or(false)
}

fn clean_git_repository_depth(
    repository: &Repository,
    entry: &Path,
    depth: u8,
) -> Result<bool> {
    if depth == 0 {
        return Ok(false);
    }
    let Ok(metadata) = repository.filesystem().metadata(entry) else {
        return Ok(false);
    };
    if metadata.is_dir() {
        return clean_git_directory(repository, entry, depth - 1);
    }
    if metadata.is_symlink() {
        // A symlinked `.git` pointing at a repository is a nested repository;
        // git's classification follows symlinks. Targets that escape the
        // repository root cannot be verified through the scoped filesystem, so
        // they are conservatively preserved.
        let Ok(target) = repository.filesystem().read_link(entry) else {
            return Ok(false);
        };
        return match clean_resolve_target(entry, &target)? {
            Some(resolved) => clean_git_repository_depth(repository, &resolved, depth - 1),
            None => Ok(true),
        };
    }
    let Ok(contents) = repository.filesystem().read(entry) else {
        // An unreadable `.git` file (e.g. mode 000) is conservatively
        // preserved, matching git's READ_GITFILE_ERR_OPEN/READ_FAILED.
        return Ok(true);
    };
    let Ok(text) = std::str::from_utf8(&contents) else {
        return Ok(false);
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir: "))
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return Ok(false);
    };
    match clean_resolve_target(entry, target.as_bytes())? {
        // git resolves a gitfile one level: the target must itself be a git
        // directory (following symlinks); a gitfile pointing at another
        // gitfile is not a repository.
        Some(resolved) => clean_git_directory(repository, &resolved, depth - 1),
        None => Ok(true),
    }
}

/// True when `path` (following symlinks) is a git directory: a valid `HEAD`
/// and `objects`/`refs`, either directly or resolved via a `commondir` file
/// (a linked worktree), mirroring git's `is_git_directory` + `get_common_dir`.
fn clean_git_directory(repository: &Repository, path: &Path, depth: u8) -> Result<bool> {
    if depth == 0 {
        return Ok(false);
    }
    let Ok(metadata) = repository.filesystem().metadata(path) else {
        return Ok(false);
    };
    if metadata.is_symlink() {
        let Ok(target) = repository.filesystem().read_link(path) else {
            return Ok(false);
        };
        return match clean_resolve_target(path, &target)? {
            Some(resolved) => clean_git_directory(repository, &resolved, depth - 1),
            None => Ok(true),
        };
    }
    if !metadata.is_dir() || !clean_valid_head(repository, &path.join("HEAD")) {
        return Ok(false);
    }
    match clean_common_dir(repository, path)? {
        Some(common) => Ok(repository.filesystem().exists(&common.join("objects"))?
            && repository.filesystem().exists(&common.join("refs"))?),
        None => Ok(true),
    }
}

/// Resolve a linked worktree's `commondir` file to the common git directory,
/// or `None` when the common directory cannot be determined and the subtree
/// must be conservatively preserved: a present but unreadable or empty
/// `commondir` leaves the common dir unverifiable (git aborts the clean in
/// this case), and a target escaping the repository root cannot be verified.
fn clean_common_dir(repository: &Repository, git_dir: &Path) -> Result<Option<PathBuf>> {
    let commondir = git_dir.join("commondir");
    let metadata = match repository.filesystem().metadata(&commondir) {
        Ok(metadata) => metadata,
        Err(Error::NotFound(_)) => return Ok(Some(git_dir.to_path_buf())),
        Err(_) => return Ok(None),
    };
    if !metadata.is_file() {
        return Ok(Some(git_dir.to_path_buf()));
    }
    let Ok(contents) = repository.filesystem().read(&commondir) else {
        return Ok(None);
    };
    let Ok(text) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };
    let target = text.lines().next().map_or("", str::trim);
    if target.is_empty() {
        return Ok(None);
    }
    clean_resolve_target(&commondir, target.as_bytes())
}

/// Resolve a symlink or `gitdir:` target relative to the directory containing
/// `entry`, producing a lexically normalized repository-scoped path, or `None`
/// when the target escapes the repository root (absolute or above the root),
/// which cannot be verified through the repository-scoped filesystem and is
/// therefore conservatively preserved.
fn clean_resolve_target(entry: &Path, target: &[u8]) -> Result<Option<PathBuf>> {
    // Detect an absolute target on the raw bytes: path conversion drops a
    // leading `/`, which would otherwise rebind the target inside the
    // repository. Absolute targets point outside the repository-scoped
    // filesystem, so they cannot be verified and are conservatively preserved.
    if target.starts_with(b"/") {
        return Ok(None);
    }
    let target = clean_worktree_path(target)?;
    let combined = if target.is_absolute() {
        target
    } else {
        entry.parent().unwrap_or_else(|| Path::new("")).join(target)
    };
    let mut normalized = PathBuf::new();
    for component in combined.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Ok(None);
                }
            }
            Component::RootDir | Component::Prefix(_) => return Ok(None),
        }
    }
    Ok(Some(normalized))
}

/// Mirror `validate_headref`: a symbolic ref to `refs/...`, a `ref:` symbolic
/// ref, or a detached hexadecimal object id.
fn clean_valid_head(repository: &Repository, head: &Path) -> bool {
    let Ok(metadata) = repository.filesystem().metadata(head) else {
        return false;
    };
    if metadata.is_symlink() {
        let Ok(target) = repository.filesystem().read_link(head) else {
            return false;
        };
        return target.starts_with(&b"refs/"[..]);
    }
    if !metadata.is_file() {
        return false;
    }
    let Ok(contents) = repository.filesystem().read(head) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&contents) else {
        return false;
    };
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix("ref:") {
        return rest.trim_start().starts_with("refs/");
    }
    let hex = text.trim_end();
    matches!(hex.len(), 40 | 64) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_clean_tree(repository: &Repository, path: &Path) -> Result<()> {
    for child in repository.filesystem().read_dir(path)? {
        let child = path.join(child);
        let directory = match repository.filesystem().metadata(&child) {
            Ok(metadata) => metadata.is_dir(),
            // A special file (FIFO/socket) reports InvalidPath from host
            // metadata; treat it as a removable non-directory.
            Err(Error::NotFound(_)) => continue,
            Err(_) => false,
        };
        if directory {
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
    use crate::{
        CommitOptions, FileSystem, HostFileSystem, InitOptions, MemoryFileSystem, Signature,
    };

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

    fn create_git_dir(filesystem: &MemoryFileSystem, git_dir: &Path) {
        filesystem
            .create_dir_all(&git_dir.join("objects"))
            .unwrap();
        filesystem.create_dir_all(&git_dir.join("refs")).unwrap();
        filesystem
            .write(&git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .unwrap();
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
        create_git_dir(&filesystem, Path::new("repo/nested/.git"));
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

    #[test]
    fn nested_git_directory_survives_recursion_below_tracked_descendant() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/nested"))
            .unwrap();
        filesystem
            .write(Path::new("repo/nested/tracked.txt"), b"tracked")
            .unwrap();
        repository.add("nested/tracked.txt").unwrap();
        create_git_dir(&filesystem, Path::new("repo/nested/.git"));
        filesystem
            .write(Path::new("repo/nested/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"nested/victim.txt".to_vec()));
        assert!(!paths(&discovered).contains(&b"nested/.git".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/nested/victim.txt")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/nested/.git/HEAD"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/nested/tracked.txt"))
                .unwrap()
        );
    }

    #[test]
    fn nested_git_gitfile_survives_recursion_below_tracked_descendant() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/nested"))
            .unwrap();
        filesystem
            .write(Path::new("repo/nested/tracked.txt"), b"tracked")
            .unwrap();
        repository.add("nested/tracked.txt").unwrap();
        filesystem
            .write(
                Path::new("repo/nested/.git"),
                b"gitdir: /external/nested-gitdir\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/nested/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"nested/victim.txt".to_vec()));
        assert!(!paths(&discovered).contains(&b"nested/.git".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/nested/victim.txt")).unwrap());
        assert!(filesystem.exists(Path::new("repo/nested/.git")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/nested/tracked.txt"))
                .unwrap()
        );
    }

    #[test]
    fn untracked_subtree_preserves_nested_repository_git_at_depth() {
        let (repository, filesystem) = fixture();
        create_git_dir(&filesystem, Path::new("repo/foo/bar/.git"));
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(!paths(&discovered).contains(&b"foo".to_vec()));
        assert!(!paths(&discovered).iter().any(|path| path.starts_with(b"foo/")));
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/bar/.git/HEAD"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/bar/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_preserves_nested_repository_gitfile_at_depth() {
        let (repository, filesystem) = fixture();
        filesystem.create_dir_all(Path::new("repo/foo/bar")).unwrap();
        filesystem
            .write(
                Path::new("repo/foo/bar/.git"),
                b"gitdir: ../bar-git\n",
            )
            .unwrap();
        create_git_dir(&filesystem, Path::new("repo/foo/bar-git"));
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(filesystem.exists(Path::new("repo/foo/bar/.git")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/foo/bar/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_removes_siblings_but_preserves_nested_repository() {
        let (repository, filesystem) = fixture();
        create_git_dir(&filesystem, Path::new("repo/foo/bar/.git"));
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/other.txt"), b"other")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo/other.txt".to_vec()));
        assert!(!paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo/other.txt")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/foo/bar/.git/HEAD"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/bar/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn exact_git_name_is_skipped_but_git_prefixed_untracked_paths_are_cleaned() {
        let (repository, filesystem) = fixture();
        filesystem
            .write(
                Path::new("repo/tracked-dir/.gitignore"),
                b"*.tmp\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked-dir/.gitmodules"), b"")
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/tracked-dir/.gitmodules.d"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/tracked-dir/.gitmodules.d/keep"),
                b"keep",
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/tracked-dir/.git"),
                b"gitdir: /external/tracked-gitdir\n",
            )
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        for path in [
            b"tracked-dir/.gitignore".to_vec(),
            b"tracked-dir/.gitmodules".to_vec(),
            b"tracked-dir/.gitmodules.d".to_vec(),
        ] {
            assert!(paths(&discovered).contains(&path), "{path:?} not found");
        }
        assert!(!paths(&discovered).contains(&b"tracked-dir/.git".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/tracked-dir/.gitignore")).unwrap());
        assert!(
            !filesystem
                .exists(Path::new("repo/tracked-dir/.gitmodules"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/tracked-dir/.gitmodules.d"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/tracked-dir/.git")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/file"))
                .unwrap()
        );
    }

    #[test]
    fn tracked_ancestor_preserves_real_git_at_depth_three() {
        let (repository, filesystem) = fixture();
        create_git_dir(&filesystem, Path::new("repo/tracked-dir/sub/.git"));
        filesystem
            .write(Path::new("repo/tracked-dir/sub/victim.txt"), b"victim")
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked-dir/other.txt"), b"other")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"tracked-dir/other.txt".to_vec()));
        assert!(
            !paths(&discovered)
                .iter()
                .any(|path| path.starts_with(b"tracked-dir/sub"))
        );
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/sub/.git/HEAD"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/sub/victim.txt"))
                .unwrap()
        );
        assert!(!filesystem.exists(Path::new("repo/tracked-dir/other.txt")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/file"))
                .unwrap()
        );
    }

    #[test]
    fn tracked_subtree_cleans_invalid_git_directory() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/tracked-dir/sub/.git"))
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked-dir/sub/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"tracked-dir/sub".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/tracked-dir/sub")).unwrap());
        assert!(
            filesystem
                .exists(Path::new("repo/tracked-dir/file"))
                .unwrap()
        );
    }

    #[test]
    fn authorized_removal_prunes_empty_parents_of_deep_nested_repository() {
        let (repository, filesystem) = fixture();
        create_git_dir(&filesystem, Path::new("repo/foo/bar/.git"));
        repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    force: true,
                    remove_nested_repositories: true,
                    dry_run: false,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(!filesystem.exists(Path::new("repo/foo/bar/.git/HEAD")).unwrap());
        assert!(!filesystem.exists(Path::new("repo/foo/bar")).unwrap());
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_removes_plain_empty_git_directory() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/bar/.git"))
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/other.txt"), b"other")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_removes_garbage_git_file() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/bar"))
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/bar/.git"), b"not a gitfile")
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_preserves_symlinked_git_repository() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/link-to-gitdir"))
            .unwrap();
        create_git_dir(&filesystem, Path::new("repo/foo/gitreal"));
        filesystem
            .create_symlink(Path::new("repo/foo/link-to-gitdir/.git"), b"../gitreal")
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/link-to-gitdir/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(!paths(&discovered).contains(&b"foo".to_vec()));
        assert!(
            !paths(&discovered)
                .iter()
                .any(|path| path.starts_with(b"foo/link-to-gitdir"))
        );
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/.git"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_removes_gitfile_with_invalid_target() {
        let (repository, filesystem) = fixture();
        filesystem.create_dir_all(Path::new("repo/foo")).unwrap();
        filesystem
            .write(
                Path::new("repo/foo/.git"),
                b"gitdir: ../missing-gitdir\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_removes_head_only_git_directory() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/.git"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/.git/HEAD"),
                b"ref: refs/heads/main\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_preserves_absolute_symlink_git_repository() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/link-to-gitdir"))
            .unwrap();
        filesystem
            .create_symlink(
                Path::new("repo/foo/link-to-gitdir/.git"),
                b"/absolute/outside-gitreal",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/link-to-gitdir/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/.git"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_preserves_absolute_gitdir_gitfile() {
        let (repository, filesystem) = fixture();
        filesystem.create_dir_all(Path::new("repo/foo")).unwrap();
        filesystem
            .write(
                Path::new("repo/foo/.git"),
                b"gitdir: /absolute/outside-gitreal\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(filesystem.exists(Path::new("repo/foo/.git")).unwrap());
        assert!(filesystem.exists(Path::new("repo/foo/victim.txt")).unwrap());
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn host_untracked_subtree_preserves_absolute_symlink_git_repository() {
        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        let gitreal = root.join("outside-gitreal");
        std::fs::create_dir_all(gitreal.join("objects")).unwrap();
        std::fs::create_dir_all(gitreal.join("refs")).unwrap();
        std::fs::write(gitreal.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("repo/foo/link-to-gitdir")).unwrap();
        std::os::unix::fs::symlink(&gitreal, root.join("repo/foo/link-to-gitdir/.git")).unwrap();
        std::fs::write(root.join("repo/foo/link-to-gitdir/victim.txt"), b"victim").unwrap();
        repository
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
        assert!(root.join("repo/foo/link-to-gitdir/.git").exists());
        assert!(root.join("repo/foo/link-to-gitdir/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_untracked_subtree_preserves_absolute_gitdir_gitfile() {
        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        let gitreal = root.join("outside-gitreal");
        std::fs::create_dir_all(gitreal.join("objects")).unwrap();
        std::fs::create_dir_all(gitreal.join("refs")).unwrap();
        std::fs::write(gitreal.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("repo/foo")).unwrap();
        std::fs::write(root.join("repo/foo/.git"), b"gitdir: /abs/outside-gitreal\n").unwrap();
        std::fs::write(root.join("repo/foo/victim.txt"), b"victim").unwrap();
        repository
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
        assert!(root.join("repo/foo/.git").exists());
        assert!(root.join("repo/foo/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_fifo_named_git_does_not_abort_clean() {
        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        std::fs::create_dir_all(root.join("repo/foo")).unwrap();
        std::process::Command::new("mkfifo")
            .arg(root.join("repo/foo/.git"))
            .status()
            .unwrap();
        std::fs::write(root.join("repo/foo/victim.txt"), b"victim").unwrap();
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
        assert!(paths(&removed).contains(&b"foo".to_vec()));
        assert!(!root.join("repo/foo").exists());
    }

    #[test]
    fn untracked_subtree_preserves_linked_worktree_git_repository() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/link-to-worktree"))
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/foo/worktree-git"))
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/foo/common-git/objects"))
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/foo/common-git/refs"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/worktree-git/HEAD"),
                b"ref: refs/heads/main\n",
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/worktree-git/commondir"),
                b"../common-git\n",
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/link-to-worktree/.git"),
                b"gitdir: ../worktree-git\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/link-to-worktree/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-worktree/.git"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-worktree/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[test]
    fn untracked_subtree_preserves_gitfile_to_symlinked_gitdir() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/link-to-gitdir"))
            .unwrap();
        create_git_dir(&filesystem, Path::new("repo/foo/gitreal"));
        filesystem
            .create_symlink(
                Path::new("repo/foo/link-to-gitdir/gitlink"),
                b"../gitreal",
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/link-to-gitdir/.git"),
                b"gitdir: gitlink\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/link-to-gitdir/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/.git"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-gitdir/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn host_untracked_subtree_preserves_linked_worktree() {
        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        std::fs::create_dir_all(root.join("repo/foo/link-to-worktree")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/worktree-git")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/common-git/objects")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/common-git/refs")).unwrap();
        std::fs::write(root.join("repo/foo/worktree-git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("repo/foo/worktree-git/commondir"), b"../common-git\n").unwrap();
        std::os::unix::fs::symlink(
            "../worktree-git",
            root.join("repo/foo/link-to-worktree/.git"),
        )
        .unwrap();
        std::fs::write(root.join("repo/foo/link-to-worktree/victim.txt"), b"victim").unwrap();
        repository
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
        // The nested repo directory survives; the `.git` symlink itself
        // remains even though the in-worktree gitdir content (`worktree-git`,
        // `common-git`) is cleaned like git does, leaving the link dangling.
        assert!(
            std::fs::symlink_metadata(root.join("repo/foo/link-to-worktree/.git"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(root.join("repo/foo/link-to-worktree/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_untracked_subtree_preserves_gitfile_to_symlinked_gitdir() {
        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        std::fs::create_dir_all(root.join("repo/foo/link-to-gitdir")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/gitreal/objects")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/gitreal/refs")).unwrap();
        std::fs::write(root.join("repo/foo/gitreal/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::os::unix::fs::symlink(
            "../gitreal",
            root.join("repo/foo/link-to-gitdir/gitlink"),
        )
        .unwrap();
        std::fs::write(root.join("repo/foo/link-to-gitdir/.git"), b"gitdir: gitlink\n").unwrap();
        std::fs::write(root.join("repo/foo/link-to-gitdir/victim.txt"), b"victim").unwrap();
        repository
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
        assert!(root.join("repo/foo/link-to-gitdir/.git").exists());
        assert!(root.join("repo/foo/link-to-gitdir/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_unreadable_git_file_is_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        std::fs::create_dir_all(root.join("repo/foo")).unwrap();
        let gitfile = root.join("repo/foo/.git");
        std::fs::write(&gitfile, b"gitdir: /somewhere/real\n").unwrap();
        std::fs::set_permissions(&gitfile, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(root.join("repo/foo/victim.txt"), b"victim").unwrap();
        repository
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
        assert!(root.join("repo/foo/.git").exists());
        assert!(root.join("repo/foo/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[test]
    fn untracked_subtree_preserves_linked_worktree_with_empty_commondir() {
        let (repository, filesystem) = fixture();
        filesystem
            .create_dir_all(Path::new("repo/foo/link-to-worktree"))
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/foo/worktree-git"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/worktree-git/HEAD"),
                b"ref: refs/heads/main\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/worktree-git/commondir"), b"")
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/link-to-worktree/.git"),
                b"gitdir: ../worktree-git\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/link-to-worktree/victim.txt"), b"victim")
            .unwrap();
        repository
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
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-worktree/.git"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/foo/link-to-worktree/victim.txt"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn host_untracked_subtree_preserves_linked_worktree_with_unreadable_commondir() {
        use std::os::unix::fs::PermissionsExt;

        let root = host_fixture_root();
        let repository =
            Repository::init(HostFileSystem::new(&root).unwrap(), "repo", &InitOptions::default())
                .unwrap();
        std::fs::write(root.join("repo/base.txt"), b"base").unwrap();
        repository.add("base.txt").unwrap();
        std::fs::create_dir_all(root.join("repo/foo/link-to-worktree")).unwrap();
        std::fs::create_dir_all(root.join("repo/foo/worktree-git")).unwrap();
        std::fs::write(root.join("repo/foo/worktree-git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        let commondir = root.join("repo/foo/worktree-git/commondir");
        std::fs::write(&commondir, b"../common-git\n").unwrap();
        std::fs::set_permissions(&commondir, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(root.join("repo/foo/link-to-worktree/.git"), b"gitdir: ../worktree-git\n").unwrap();
        std::fs::write(root.join("repo/foo/link-to-worktree/victim.txt"), b"victim").unwrap();
        repository
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
        assert!(root.join("repo/foo/link-to-worktree/.git").exists());
        assert!(root.join("repo/foo/link-to-worktree/victim.txt").exists());
        assert!(root.join("repo/foo").exists());
    }

    #[test]
    fn untracked_subtree_removes_gitfile_to_gitfile_chain() {
        let (repository, filesystem) = fixture();
        filesystem.create_dir_all(Path::new("repo/foo/bar")).unwrap();
        create_git_dir(&filesystem, Path::new("repo/foo/gitreal"));
        filesystem
            .write(
                Path::new("repo/foo/bar/gitfile2"),
                b"gitdir: ../gitreal\n",
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/foo/bar/.git"),
                b"gitdir: gitfile2\n",
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/foo/bar/victim.txt"), b"victim")
            .unwrap();
        let discovered = repository
            .clean::<&str>(
                &[],
                &CleanOptions {
                    directories: true,
                    ..CleanOptions::default()
                },
            )
            .unwrap();
        assert!(paths(&discovered).contains(&b"foo".to_vec()));
        repository
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
        assert!(!filesystem.exists(Path::new("repo/foo")).unwrap());
    }

    fn host_fixture_root() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-clean-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
