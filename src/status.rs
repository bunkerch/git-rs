//! Repository status from HEAD, index, and worktree state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use crate::{Error, IgnoreMatcher, IndexEntry, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    TypeChanged,
    Unmerged,
    Untracked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
    path: Vec<u8>,
    index: Option<ChangeKind>,
    worktree: Option<ChangeKind>,
}

impl StatusEntry {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    #[must_use]
    pub const fn index_change(&self) -> Option<ChangeKind> {
        self.index
    }

    #[must_use]
    pub const fn worktree_change(&self) -> Option<ChangeKind> {
        self.worktree
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryStatus {
    entries: Vec<StatusEntry>,
}

impl RepositoryStatus {
    #[must_use]
    pub fn entries(&self) -> &[StatusEntry] {
        &self.entries
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct StatusOptions {
    pub include_untracked: bool,
    pub max_object_size: usize,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            include_untracked: true,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Compare HEAD to the index and the index to the worktree.
    ///
    /// # Errors
    /// Returns an error for corrupt objects/indexes, inaccessible worktree
    /// state, bare repositories, or paths unsupported by the host adapter.
    #[allow(clippy::too_many_lines)]
    pub fn status(&self, options: &StatusOptions) -> Result<RepositoryStatus> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("status requires a non-bare repository".into())
        })?;
        let index = self.read_index()?;
        let head = match self.resolve_reference("HEAD") {
            Ok(commit_id) => {
                let commit = self.read_commit(commit_id, options.max_object_size)?;
                self.flattened_tree(commit.tree(), options.max_object_size)?
                    .into_iter()
                    .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
                    .collect::<BTreeMap<_, _>>()
            }
            Err(Error::NotFound(_)) => BTreeMap::new(),
            Err(error) => return Err(error),
        };

        let mut by_path = BTreeMap::<Vec<u8>, StatusEntry>::new();
        let mut index_stage_zero = BTreeMap::new();
        let mut tracked = BTreeSet::new();
        let mut unmerged = BTreeSet::new();
        for entry in index.entries() {
            tracked.insert(entry.path().to_vec());
            if entry.stage() == 0 {
                index_stage_zero.insert(entry.path().to_vec(), entry);
            } else {
                unmerged.insert(entry.path().to_vec());
            }
        }

        for path in head.keys().chain(index_stage_zero.keys()) {
            if unmerged.contains(path) {
                continue;
            }
            let change = match (head.get(path), index_stage_zero.get(path)) {
                (None, Some(_)) => Some(ChangeKind::Added),
                (Some(_), None) => Some(ChangeKind::Deleted),
                (Some((head_mode, _)), Some(index_entry)) if *head_mode != index_entry.mode() => {
                    Some(ChangeKind::TypeChanged)
                }
                (Some((_, head_id)), Some(index_entry)) if *head_id != index_entry.id() => {
                    Some(ChangeKind::Modified)
                }
                _ => None,
            };
            if let Some(change) = change {
                by_path.entry(path.clone()).or_insert(StatusEntry {
                    path: path.clone(),
                    index: Some(change),
                    worktree: None,
                });
            }
        }
        for path in unmerged {
            by_path.insert(
                path.clone(),
                StatusEntry {
                    path,
                    index: Some(ChangeKind::Unmerged),
                    worktree: None,
                },
            );
        }

        for (path, entry) in &index_stage_zero {
            if entry.assume_valid() || entry.skip_worktree() {
                continue;
            }
            let full_path = work_tree.join(worktree_path(path)?);
            let change = match self.filesystem().metadata(&full_path) {
                Err(Error::NotFound(_)) => Some(ChangeKind::Deleted),
                Err(error) => return Err(error),
                Ok(metadata) => worktree_change(self, entry, &full_path, metadata)?,
            };
            if let Some(change) = change {
                let status = by_path.entry(path.clone()).or_insert(StatusEntry {
                    path: path.clone(),
                    index: None,
                    worktree: None,
                });
                status.worktree = Some(change);
            }
        }

        if options.include_untracked {
            let gitlinks = index_stage_zero
                .iter()
                .filter(|(_, entry)| entry.mode() == 0o160_000)
                .map(|(path, _)| path.clone())
                .collect::<BTreeSet<_>>();
            let mut untracked = Vec::new();
            let mut ignores = self.ignore_matcher()?;
            self.collect_untracked(
                work_tree,
                Path::new(""),
                &tracked,
                &gitlinks,
                &mut ignores,
                &mut untracked,
            )?;
            for path in untracked {
                by_path.entry(path.clone()).or_insert(StatusEntry {
                    path,
                    index: None,
                    worktree: Some(ChangeKind::Untracked),
                });
            }
        }

        Ok(RepositoryStatus {
            entries: by_path.into_values().collect(),
        })
    }

    fn collect_untracked(
        &self,
        work_tree: &Path,
        relative: &Path,
        tracked: &BTreeSet<Vec<u8>>,
        gitlinks: &BTreeSet<Vec<u8>>,
        ignores: &mut IgnoreMatcher,
        output: &mut Vec<Vec<u8>>,
    ) -> Result<()> {
        ignores.add_worktree_patterns(self, work_tree, &index_path(relative)?)?;
        let directory = work_tree.join(relative);
        for child in self.filesystem().read_dir(&directory)? {
            let child_relative = relative.join(child);
            if child_relative == Path::new(".git")
                || work_tree.join(&child_relative) == self.git_dir()
            {
                continue;
            }
            let path = index_path(&child_relative)?;
            if gitlinks.contains(&path) {
                continue;
            }
            let metadata = self
                .filesystem()
                .metadata(&work_tree.join(&child_relative))?;
            if metadata.is_dir() {
                if !ignores.is_ignored(&path, true) {
                    self.collect_untracked(
                        work_tree,
                        &child_relative,
                        tracked,
                        gitlinks,
                        ignores,
                        output,
                    )?;
                }
            } else if !tracked.contains(&path) && !ignores.is_ignored(&path, false) {
                output.push(path);
            }
        }
        Ok(())
    }
}

pub(crate) fn worktree_change(
    repository: &Repository,
    entry: &IndexEntry,
    path: &Path,
    metadata: crate::Metadata,
) -> Result<Option<ChangeKind>> {
    if metadata.is_dir() {
        return Ok((entry.mode() != 0o160_000).then_some(ChangeKind::TypeChanged));
    }
    let (mode, contents) = if metadata.is_symlink() {
        (0o120_000, repository.filesystem().read_link(path)?)
    } else if metadata.is_file() {
        (
            if metadata.is_executable() {
                0o100_755
            } else {
                0o100_644
            },
            repository.filesystem().read(path)?,
        )
    } else {
        return Ok(Some(ChangeKind::TypeChanged));
    };
    if mode != entry.mode() {
        return Ok(Some(ChangeKind::TypeChanged));
    }
    Ok(
        (ObjectId::compute(ObjectKind::Blob, &contents) != entry.id())
            .then_some(ChangeKind::Modified),
    )
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn index_path(path: &Path) -> Result<Vec<u8>> {
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
fn index_path(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/").into_bytes())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

pub(crate) use crate::worktree::worktree_path;

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::str::FromStr;

    use super::*;
    use crate::{
        CommitBuilder, FileSystem, InitOptions, MemoryFileSystem, PreviousValue, ReferenceName,
        Signature,
    };

    fn committed_repository() -> (Repository, MemoryFileSystem) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/tracked"), b"old").unwrap();
        repository.add(".").unwrap();
        let tree = repository
            .write_index_tree(&repository.read_index().unwrap())
            .unwrap();
        let signature = Signature::new("Test", "test@example.com", 0, 0).unwrap();
        let commit = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .message(b"base\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                commit,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        (repository, fs)
    }

    #[test]
    fn reports_clean_modified_deleted_and_untracked_worktree_states() {
        let (repository, fs) = committed_repository();
        assert!(
            repository
                .status(&StatusOptions::default())
                .unwrap()
                .is_clean()
        );

        fs.write(Path::new("repo/tracked"), b"new").unwrap();
        fs.write(Path::new("repo/untracked"), b"new").unwrap();
        let status = repository.status(&StatusOptions::default()).unwrap();
        assert_eq!(status.entries().len(), 2);
        assert_eq!(status.entries()[0].path(), b"tracked");
        assert_eq!(
            status.entries()[0].worktree_change(),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            status.entries()[1].worktree_change(),
            Some(ChangeKind::Untracked)
        );

        fs.remove_file(Path::new("repo/tracked")).unwrap();
        assert_eq!(
            repository
                .status(&StatusOptions::default())
                .unwrap()
                .entries()[0]
                .worktree_change(),
            Some(ChangeKind::Deleted)
        );
    }

    #[test]
    fn excludes_ignored_untracked_paths_with_nested_overrides() {
        let (repository, fs) = committed_repository();
        fs.write(
            Path::new("repo/.gitignore"),
            b"*.log\nbuild/\n!important.log\n",
        )
        .unwrap();
        fs.create_dir_all(Path::new("repo/nested")).unwrap();
        fs.write(Path::new("repo/nested/.gitignore"), b"!keep.log\n")
            .unwrap();
        fs.write(Path::new("repo/error.log"), b"ignored").unwrap();
        fs.write(Path::new("repo/important.log"), b"visible")
            .unwrap();
        fs.write(Path::new("repo/nested/drop.log"), b"ignored")
            .unwrap();
        fs.write(Path::new("repo/nested/keep.log"), b"visible")
            .unwrap();
        fs.create_dir_all(Path::new("repo/build")).unwrap();
        fs.write(Path::new("repo/build/output"), b"ignored")
            .unwrap();
        fs.write(Path::new("repo/.git/info/exclude"), b"from-info.tmp\n")
            .unwrap();
        fs.write(Path::new("repo/from-info.tmp"), b"ignored")
            .unwrap();

        let paths = repository
            .status(&StatusOptions::default())
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| entry.worktree_change() == Some(ChangeKind::Untracked))
            .map(|entry| entry.path().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                b".gitignore".to_vec(),
                b"important.log".to_vec(),
                b"nested/.gitignore".to_vec(),
                b"nested/keep.log".to_vec(),
            ]
        );
    }

    #[test]
    fn reports_staged_changes_against_head() {
        let (repository, fs) = committed_repository();
        fs.write(Path::new("repo/tracked"), b"changed").unwrap();
        fs.write(Path::new("repo/added"), b"added").unwrap();
        repository.add(".").unwrap();
        let status = repository.status(&StatusOptions::default()).unwrap();
        assert_eq!(
            status
                .entries()
                .iter()
                .map(|entry| (entry.path(), entry.index_change()))
                .collect::<Vec<_>>(),
            [
                (b"added".as_slice(), Some(ChangeKind::Added)),
                (b"tracked".as_slice(), Some(ChangeKind::Modified)),
            ]
        );
    }

    #[test]
    fn reports_unmerged_stages_once() {
        let (repository, _) = committed_repository();
        let id = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        let index = crate::Index::new(
            crate::IndexVersion::V2,
            vec![
                IndexEntry::with_stage(
                    b"conflict".to_vec(),
                    0o100_644,
                    id,
                    crate::StatData::default(),
                    1,
                )
                .unwrap(),
                IndexEntry::with_stage(
                    b"conflict".to_vec(),
                    0o100_644,
                    id,
                    crate::StatData::default(),
                    2,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        repository.write_index(&index).unwrap();
        let status = repository
            .status(&StatusOptions {
                include_untracked: false,
                ..StatusOptions::default()
            })
            .unwrap();
        assert_eq!(status.entries().len(), 2);
        assert_eq!(
            status
                .entries()
                .iter()
                .find(|entry| entry.path() == b"conflict")
                .unwrap()
                .index_change(),
            Some(ChangeKind::Unmerged)
        );
    }

    #[test]
    fn worktree_path_consistently_rejects_backslashes() {
        assert!(worktree_path(b"..\\pwned").is_err());
        assert!(worktree_path(b"foo\\bar").is_err());
        assert!(worktree_path(b"deps/lib").is_ok());
    }
}
