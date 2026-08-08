//! Restore literal paths from the index, `HEAD`, or an explicit treeish.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::worktree::has_symlink_leading_path;
use crate::worktree::worktree_path as restore_worktree_path;
use crate::{Error, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result, StatData};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RestoreTarget {
    #[default]
    Worktree,
    Index,
    Both,
}

/// Source, destination-layer, collision, and resource policy for restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreOptions {
    /// Commit or tree to restore. `None` means index for worktree-only and
    /// `HEAD` for index/both.
    pub source: Option<ObjectId>,
    pub target: RestoreTarget,
    /// Permit overwriting untracked file/directory obstructions.
    pub force: bool,
    pub ignore_unmatched: bool,
    pub dry_run: bool,
    pub max_object_size: usize,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            source: None,
            target: RestoreTarget::Worktree,
            force: false,
            ignore_unmatched: false,
            dry_run: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Restore literal file or directory-prefix selections into the worktree,
    /// index, or both layers.
    ///
    /// Worktree-only restore defaults to the current index. Index or combined
    /// restore defaults to `HEAD`. An explicit source may name a commit or tree.
    /// Selected tracked modifications are intentionally discarded; unrelated
    /// paths remain untouched. Entry-dependent index extensions are invalidated.
    ///
    /// # Errors
    /// Returns an error for no/unsafe/unmatched paths, malformed or oversized
    /// sources, unmerged index-as-source paths, untracked obstructions without
    /// force, unsupported tree modes, index contention, or storage failures.
    #[allow(clippy::too_many_lines)]
    pub fn restore_paths<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: &RestoreOptions,
    ) -> Result<Vec<Vec<u8>>> {
        if paths.is_empty() {
            return Err(Error::InvalidPath(PathBuf::new()));
        }
        let restore_worktree = matches!(
            options.target,
            RestoreTarget::Worktree | RestoreTarget::Both
        );
        let restore_index = matches!(options.target, RestoreTarget::Index | RestoreTarget::Both);
        let work_tree = if restore_worktree {
            Some(self.work_tree().ok_or_else(|| {
                Error::InvalidRepository("worktree restore requires a non-bare repository".into())
            })?)
        } else {
            None
        };
        let current = self.read_index()?;
        let source = if options.source.is_none() && !restore_index {
            current.clone()
        } else {
            self.restore_source_index(options.source, current.version(), options.max_object_size)?
        };
        let requested = paths
            .iter()
            .map(|path| normalize_restore_path(path.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let universe = current
            .entries()
            .iter()
            .map(IndexEntry::path)
            .chain(source.entries().iter().map(IndexEntry::path))
            .collect::<Vec<_>>();
        let mut selected = BTreeSet::new();
        for prefix in &requested {
            let matches = universe
                .iter()
                .filter(|candidate| restore_path_selected(candidate, prefix))
                .copied()
                .collect::<Vec<_>>();
            if matches.is_empty() && !options.ignore_unmatched {
                return Err(Error::NotFound(restore_worktree_path(prefix)?));
            }
            selected.extend(matches.into_iter().map(<[u8]>::to_vec));
        }
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let desired = source
            .entries()
            .iter()
            .filter(|entry| entry.stage() == 0 && selected.contains(entry.path()))
            .map(|entry| (entry.path().to_vec(), entry.clone()))
            .collect::<BTreeMap<_, _>>();
        if restore_worktree {
            for path in &selected {
                if source
                    .entries()
                    .iter()
                    .any(|entry| entry.path() == path && entry.stage() != 0)
                {
                    return Err(Error::InvalidRepository(format!(
                        "cannot restore unmerged index path `{}`",
                        String::from_utf8_lossy(path)
                    )));
                }
            }
            let root = work_tree.ok_or_else(|| {
                Error::InvalidRepository("worktree restore requires a non-bare repository".into())
            })?;
            for path in &requested {
                let relative = restore_worktree_path(path)?;
                if has_symlink_leading_path(self.filesystem(), root, &relative)? {
                    return Err(Error::BeyondSymbolicLink(relative));
                }
            }
            for path in &selected {
                let relative = restore_worktree_path(path)?;
                if has_symlink_leading_path(self.filesystem(), root, &relative)? {
                    return Err(Error::BeyondSymbolicLink(relative));
                }
            }
            self.preflight_restore_worktree(root, &current, &selected, &desired, options.force)?;
        }
        let restored = selected.iter().cloned().collect::<Vec<_>>();
        if options.dry_run {
            return Ok(restored);
        }
        if let Some(root) = work_tree {
            self.apply_restore_worktree(
                root,
                &current,
                &selected,
                &desired,
                options.max_object_size,
                options.force,
            )?;
        }
        if restore_index {
            let mut entries = current
                .entries()
                .iter()
                .filter(|entry| !selected.contains(entry.path()))
                .cloned()
                .collect::<Vec<_>>();
            entries.extend(desired.into_values());
            self.write_index(&Index::new(current.version(), entries)?)?;
        }
        Ok(restored)
    }

    fn restore_source_index(
        &self,
        source: Option<ObjectId>,
        version: crate::IndexVersion,
        max_object_size: usize,
    ) -> Result<Index> {
        let tree = match source {
            Some(id) => match self.read_object(id, max_object_size)?.kind() {
                ObjectKind::Commit => self.read_commit(id, max_object_size)?.tree(),
                ObjectKind::Tree => id,
                _ => return Err(Error::InvalidTree("restore source is not a treeish".into())),
            },
            None => match self.resolve_reference("HEAD") {
                Ok(id) => self.read_commit(id, max_object_size)?.tree(),
                Err(Error::NotFound(_)) => {
                    return Index::new(version, Vec::new());
                }
                Err(error) => return Err(error),
            },
        };
        let entries = self
            .flattened_tree(tree, max_object_size)?
            .into_iter()
            .map(|entry| IndexEntry::new(entry.path, entry.raw_mode, entry.id, StatData::default()))
            .collect::<Result<Vec<_>>>()?;
        Index::new(version, entries)
    }

    fn preflight_restore_worktree(
        &self,
        root: &Path,
        current: &Index,
        selected: &BTreeSet<Vec<u8>>,
        desired: &BTreeMap<Vec<u8>, IndexEntry>,
        force: bool,
    ) -> Result<()> {
        let tracked = current
            .entries()
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let mut conflicts = BTreeSet::new();
        for (path, entry) in desired {
            let relative = restore_worktree_path(path)?;
            let full = root.join(&relative);
            for parent in restore_parent_paths(path) {
                let parent_full = root.join(restore_worktree_path(parent)?);
                match self.filesystem().metadata(&parent_full) {
                    Ok(metadata) if !metadata.is_dir() && !selected.contains(parent) && !force => {
                        conflicts.insert(parent.to_vec());
                    }
                    Ok(_) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            match self.filesystem().metadata(&full) {
                Ok(metadata) if metadata.is_dir() && entry.mode() != 0o160_000 => {
                    if !force {
                        conflicts.insert(path.clone());
                    }
                }
                Ok(_) if !tracked.contains(path) && !force => {
                    conflicts.insert(path.clone());
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(Error::CheckoutConflict(
                conflicts
                    .into_iter()
                    .map(|path| String::from_utf8_lossy(&path).into_owned())
                    .collect(),
            ))
        }
    }

    fn apply_restore_worktree(
        &self,
        root: &Path,
        current: &Index,
        selected: &BTreeSet<Vec<u8>>,
        desired: &BTreeMap<Vec<u8>, IndexEntry>,
        max_object_size: usize,
        force: bool,
    ) -> Result<()> {
        let current_paths = current
            .entries()
            .iter()
            .filter(|entry| selected.contains(entry.path()))
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        for path in current_paths.iter().rev() {
            let full = root.join(restore_worktree_path(path)?);
            match self.filesystem().metadata(&full) {
                Ok(metadata) if metadata.is_dir() => remove_restore_tree(self, &full)?,
                Ok(_) => self.filesystem().remove_file(&full)?,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            prune_restore_parents(self, root, restore_worktree_path(path)?.parent())?;
        }
        for (path, entry) in desired {
            let full = root.join(restore_worktree_path(path)?);
            for parent in restore_parent_paths(path) {
                let parent = root.join(restore_worktree_path(parent)?);
                if self
                    .filesystem()
                    .metadata(&parent)
                    .is_ok_and(|metadata| !metadata.is_dir())
                {
                    if !force {
                        return Err(Error::CheckoutConflict(vec![
                            String::from_utf8_lossy(path).into_owned(),
                        ]));
                    }
                    self.filesystem().remove_file(&parent)?;
                }
            }
            match self.filesystem().metadata(&full) {
                Ok(metadata) if metadata.is_dir() => remove_restore_tree(self, &full)?,
                Ok(_) => self.filesystem().remove_file(&full)?,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            if let Some(parent) = full.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            let object = self.read_object(entry.id(), max_object_size)?;
            if entry.mode() == 0o160_000 {
                self.filesystem().create_dir_all(&full)?;
            } else if object.kind() != ObjectKind::Blob {
                return Err(Error::InvalidObject("restore entry is not a blob".into()));
            } else if entry.mode() == 0o120_000 {
                self.filesystem().create_symlink(&full, object.data())?;
            } else {
                self.filesystem().write(&full, object.data())?;
                self.filesystem()
                    .set_executable(&full, entry.mode() == 0o100_755)?;
            }
        }
        Ok(())
    }
}

fn restore_parent_paths(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'/').then_some(&path[..index]))
}

fn restore_path_selected(candidate: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || candidate == prefix
        || (candidate.starts_with(prefix) && candidate.get(prefix.len()) == Some(&b'/'))
}

fn remove_restore_tree(repository: &Repository, path: &Path) -> Result<()> {
    for child in repository.filesystem().read_dir(path)? {
        let child = path.join(child);
        if repository.filesystem().metadata(&child)?.is_dir() {
            remove_restore_tree(repository, &child)?;
        } else {
            repository.filesystem().remove_file(&child)?;
        }
    }
    repository.filesystem().remove_dir(path)
}

fn prune_restore_parents(
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

fn normalize_restore_path(path: &Path) -> Result<Vec<u8>> {
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
    restore_index_path(&normalized)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn restore_index_path(path: &Path) -> Result<Vec<u8>> {
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
fn restore_index_path(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|path| path.replace('\\', "/").into_bytes())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitOptions, FileSystem, HostFileSystem, IndexVersion, InitOptions, MemoryFileSystem,
        Signature, StatusOptions,
    };

    fn fixture() -> (Repository, MemoryFileSystem, Signature, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem.create_dir_all(Path::new("repo/dir")).unwrap();
        filesystem.write(Path::new("repo/file"), b"base").unwrap();
        filesystem.write(Path::new("repo/dir/a"), b"a").unwrap();
        repository.add(".").unwrap();
        let signature = Signature::new("Restore", "restore@example.com", 100, 0).unwrap();
        let base = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        (repository, filesystem, signature, base)
    }

    #[test]
    fn worktree_defaults_to_index_and_staged_defaults_to_head() {
        let (repository, filesystem, _, _) = fixture();
        filesystem.write(Path::new("repo/file"), b"staged").unwrap();
        repository.add("file").unwrap();
        let staged = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.path() == b"file")
            .unwrap()
            .id();
        filesystem.write(Path::new("repo/file"), b"local").unwrap();
        repository
            .restore_paths(&["file"], &RestoreOptions::default())
            .unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"staged");
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .find(|entry| entry.path() == b"file")
                .unwrap()
                .id(),
            staged
        );

        repository
            .restore_paths(
                &["file"],
                &RestoreOptions {
                    target: RestoreTarget::Index,
                    ..RestoreOptions::default()
                },
            )
            .unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"staged");
        let status = repository.status(&StatusOptions::default()).unwrap();
        assert_eq!(status.entries().len(), 1);
        assert!(status.entries()[0].index_change().is_none());
        assert!(status.entries()[0].worktree_change().is_some());

        repository
            .restore_paths(
                &["file"],
                &RestoreOptions {
                    target: RestoreTarget::Both,
                    ..RestoreOptions::default()
                },
            )
            .unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"base");
        assert!(
            repository
                .status(&StatusOptions::default())
                .unwrap()
                .is_clean()
        );
    }

    #[test]
    fn explicit_commit_restores_directory_additions_modifications_and_deletions() {
        let (repository, filesystem, signature, base) = fixture();
        filesystem.write(Path::new("repo/file"), b"second").unwrap();
        filesystem.remove_file(Path::new("repo/dir/a")).unwrap();
        filesystem.write(Path::new("repo/new"), b"new").unwrap();
        repository.add(".").unwrap();
        repository
            .commit_index(b"second", &signature, &signature, &CommitOptions::default())
            .unwrap();
        repository
            .restore_paths(
                &["."],
                &RestoreOptions {
                    source: Some(base),
                    target: RestoreTarget::Both,
                    ..RestoreOptions::default()
                },
            )
            .unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"base");
        assert_eq!(filesystem.read(Path::new("repo/dir/a")).unwrap(), b"a");
        assert!(!filesystem.exists(Path::new("repo/new")).unwrap());
        let tree = repository
            .write_index_tree(&repository.read_index().unwrap())
            .unwrap();
        assert_eq!(tree, repository.read_commit(base, 4096).unwrap().tree());
    }

    #[test]
    fn staged_restore_clears_unmerged_entries_without_touching_worktree() {
        let (repository, filesystem, _, _) = fixture();
        let blob = repository
            .write_object(ObjectKind::Blob, b"conflict")
            .unwrap();
        let mut entries = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| entry.path() != b"file")
            .cloned()
            .collect::<Vec<_>>();
        entries
            .push(IndexEntry::with_stage("file", 0o100_644, blob, StatData::default(), 1).unwrap());
        entries
            .push(IndexEntry::with_stage("file", 0o100_644, blob, StatData::default(), 2).unwrap());
        entries
            .push(IndexEntry::with_stage("file", 0o100_644, blob, StatData::default(), 3).unwrap());
        repository
            .write_index(&Index::new(IndexVersion::V2, entries).unwrap())
            .unwrap();
        filesystem
            .write(Path::new("repo/file"), b"markers")
            .unwrap();
        repository
            .restore_paths(
                &["file"],
                &RestoreOptions {
                    target: RestoreTarget::Index,
                    ..RestoreOptions::default()
                },
            )
            .unwrap();
        let entries = repository.read_index().unwrap();
        let file = entries
            .entries()
            .iter()
            .filter(|entry| entry.path() == b"file")
            .collect::<Vec<_>>();
        assert_eq!(file.len(), 1);
        assert_eq!(file[0].stage(), 0);
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"markers");
    }

    #[test]
    fn protects_untracked_source_addition_unless_forced() {
        let (repository, filesystem, signature, base) = fixture();
        filesystem.remove_file(Path::new("repo/file")).unwrap();
        repository.add("file").unwrap();
        repository
            .commit_index(b"delete", &signature, &signature, &CommitOptions::default())
            .unwrap();
        filesystem
            .write(Path::new("repo/file"), b"untracked")
            .unwrap();
        let options = RestoreOptions {
            source: Some(base),
            target: RestoreTarget::Worktree,
            ..RestoreOptions::default()
        };
        assert!(matches!(
            repository.restore_paths(&["file"], &options),
            Err(Error::CheckoutConflict(_))
        ));
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"untracked"
        );
        repository
            .restore_paths(
                &["file"],
                &RestoreOptions {
                    force: true,
                    ..options
                },
            )
            .unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"base");
        assert!(
            !repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.path() == b"file")
        );
    }

    #[test]
    fn dry_run_and_ignore_unmatched_leave_layers_unchanged() {
        let (repository, filesystem, _, _) = fixture();
        filesystem.write(Path::new("repo/file"), b"local").unwrap();
        let before = repository.read_index().unwrap();
        assert_eq!(
            repository
                .restore_paths(
                    &["file", "absent"],
                    &RestoreOptions {
                        ignore_unmatched: true,
                        dry_run: true,
                        ..RestoreOptions::default()
                    }
                )
                .unwrap(),
            vec![b"file".to_vec()]
        );
        assert_eq!(repository.read_index().unwrap(), before);
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"local");
    }

    #[test]
    fn rejects_restore_through_a_symlinked_directory_before_deleting() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"top-secret\n").unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        let index = repository.read_index().unwrap();
        let secret = ObjectId::compute(ObjectKind::Blob, b"top-secret\n");
        let mut entries = index.entries().to_vec();
        entries.push(
            IndexEntry::new("link/secret.txt", 0o100_644, secret, StatData::default()).unwrap(),
        );
        repository
            .write_index(&Index::new(index.version(), entries).unwrap())
            .unwrap();

        assert!(matches!(
            repository.restore_paths(
                &["link/secret.txt"],
                &RestoreOptions {
                    force: true,
                    ..RestoreOptions::default()
                }
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(
            outside.join("secret.txt").exists(),
            "external file must not be deleted"
        );
    }
}
