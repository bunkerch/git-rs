//! Save and restore worktree and index state using Git-compatible stash commits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{
    CheckoutOptions, CommitBuilder, EntryMode, Error, Index, IndexEntry, ObjectId, ObjectKind,
    PreviousValue, ReferenceName, ReferenceTarget, Repository, Result, Signature, StatData,
    StatusOptions,
};

/// State selection and resource bounds for creating a stash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashPushOptions {
    pub include_untracked: bool,
    pub message: Option<Vec<u8>>,
    pub max_object_size: usize,
}

impl Default for StashPushOptions {
    fn default() -> Self {
        Self {
            include_untracked: false,
            message: None,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// Restoration policy and read bounds for applying a stash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashApplyOptions {
    /// Reconstruct the saved index in addition to worktree changes.
    pub reinstate_index: bool,
    pub max_object_size: usize,
}

impl Default for StashApplyOptions {
    fn default() -> Self {
        Self {
            reinstate_index: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StashApplyResult {
    Applied,
    Conflicted { paths: Vec<Vec<u8>> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashEntry {
    index: usize,
    commit: ObjectId,
    message: Vec<u8>,
}

impl StashEntry {
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub const fn commit(&self) -> ObjectId {
        self.commit
    }

    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }
}

struct StashData {
    commit: ObjectId,
    base_tree: ObjectId,
    index_tree: ObjectId,
    worktree_tree: ObjectId,
    untracked_tree: Option<ObjectId>,
}

impl Repository {
    /// Save tracked worktree and index changes, optionally including untracked
    /// files, then restore the checked-out `HEAD` tree.
    ///
    /// The resulting commit uses Git's stash topology: base, index, and
    /// optional untracked commits are parents of the worktree commit.
    ///
    /// # Errors
    /// Returns an error for an unborn or bare repository, no selected changes,
    /// unresolved entries, unsafe paths, corrupt objects, ref races, checkout
    /// conflicts, or storage failures.
    pub fn stash_push(
        &self,
        options: &StashPushOptions,
        committer: &Signature,
    ) -> Result<ObjectId> {
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("stash requires a working tree".into()))?;
        let head = self
            .resolve_reference("HEAD")
            .map_err(|error| match error {
                Error::NotFound(_) => Error::InvalidRepository(
                    "cannot stash before creating the initial commit".into(),
                ),
                other => other,
            })?;
        let base = self.read_commit(head, options.max_object_size)?;
        let status = self.status(&StatusOptions {
            include_untracked: options.include_untracked,
            max_object_size: options.max_object_size,
        })?;
        if status.is_clean() {
            return Err(Error::InvalidRepository("no local changes to save".into()));
        }
        let index = self.read_index()?;
        let index_tree = self.write_index_tree(&index)?;
        let worktree_index = self.snapshot_tracked_worktree(&index)?;
        let worktree_tree = self.write_index_tree(&worktree_index)?;
        let untracked_paths = status
            .entries()
            .iter()
            .filter(|entry| entry.worktree_change() == Some(crate::ChangeKind::Untracked))
            .map(|entry| entry.path().to_vec())
            .collect::<Vec<_>>();
        let untracked_tree = if options.include_untracked && !untracked_paths.is_empty() {
            Some(self.snapshot_paths(&untracked_paths, index.version())?)
        } else {
            None
        };

        let description = self.stash_description(head, &base);
        let index_commit = self.write_commit(
            &CommitBuilder::new(index_tree, committer.clone(), committer.clone())
                .parent(head)
                .message(format!("index on {description}\n").into_bytes())
                .build(),
        )?;
        let untracked_commit = untracked_tree
            .map(|tree| {
                self.write_commit(
                    &CommitBuilder::new(tree, committer.clone(), committer.clone())
                        .message(format!("untracked files on {description}\n").into_bytes())
                        .build(),
                )
            })
            .transpose()?;
        let message = options.message.clone().map_or_else(
            || format!("WIP on {description}\n").into_bytes(),
            |mut message| {
                if !message.ends_with(b"\n") {
                    message.push(b'\n');
                }
                message
            },
        );
        let mut builder = CommitBuilder::new(worktree_tree, committer.clone(), committer.clone())
            .parent(head)
            .parent(index_commit);
        if let Some(untracked) = untracked_commit {
            builder = builder.parent(untracked);
        }
        let stash = self.write_commit(&builder.message(message.clone()).build())?;
        let stash_ref = ReferenceName::new("refs/stash")?;
        let previous = match self.resolve_reference(stash_ref.as_str()) {
            Ok(old) => PreviousValue::MustExist(old),
            Err(Error::NotFound(_)) => PreviousValue::MustNotExist,
            Err(error) => return Err(error),
        };
        self.update_reference_with_reflog(
            &stash_ref,
            stash,
            previous,
            committer,
            message.strip_suffix(b"\n").unwrap_or(&message),
        )?;

        self.checkout_tree(
            base.tree(),
            &CheckoutOptions {
                force: true,
                max_object_size: options.max_object_size,
            },
        )?;
        for path in &untracked_paths {
            remove_worktree_file(self, work_tree, path)?;
        }
        Ok(stash)
    }

    /// Return stash reflog entries newest first (`stash@{0}` first).
    ///
    /// # Errors
    /// Returns an error for a malformed reflog or storage failure.
    pub fn stashes(&self) -> Result<Vec<StashEntry>> {
        let mut entries = self.read_reflog("refs/stash")?;
        entries.reverse();
        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| StashEntry {
                index,
                commit: entry.new_id(),
                message: entry.message().to_vec(),
            })
            .collect())
    }

    /// Apply `stash@{index}` without removing it.
    ///
    /// Conflicts are materialized in the worktree and index stages 1–3. When
    /// `reinstate_index` is false, restored changes are unstaged except for
    /// paths newly added by the stash, matching Git's apply behavior.
    ///
    /// # Errors
    /// Returns an error for a missing/malformed stash, unresolved current
    /// index, unsafe untracked collision, merge/storage failure, or bare repo.
    pub fn stash_apply(
        &self,
        index: usize,
        options: &StashApplyOptions,
    ) -> Result<StashApplyResult> {
        let stash = self.read_stash(index, options.max_object_size)?;
        self.apply_stash_data(&stash, options)
    }

    /// Apply and remove `stash@{index}` only when application is clean.
    ///
    /// # Errors
    /// Returns the same errors as apply or drop. A conflicted stash remains in
    /// the reflog and is returned as [`StashApplyResult::Conflicted`].
    pub fn stash_pop(&self, index: usize, options: &StashApplyOptions) -> Result<StashApplyResult> {
        let stash = self.read_stash(index, options.max_object_size)?;
        let result = self.apply_stash_data(&stash, options)?;
        if result == StashApplyResult::Applied {
            self.stash_drop(index)?;
        }
        Ok(result)
    }

    /// Remove one stash reflog entry and move/delete `refs/stash` as needed.
    ///
    /// # Errors
    /// Returns an error for an out-of-range index, malformed reflog, ref race,
    /// lock contention, or storage failure.
    pub fn stash_drop(&self, index: usize) -> Result<ObjectId> {
        let listed = self.stashes()?;
        let removed = listed
            .get(index)
            .ok_or_else(|| Error::NotFound(PathBuf::from(format!("stash@{{{index}}}"))))?
            .commit;
        let mut entries = self.read_reflog("refs/stash")?;
        let position = entries
            .len()
            .checked_sub(index + 1)
            .ok_or_else(|| Error::NotFound(PathBuf::from(format!("stash@{{{index}}}"))))?;
        entries.remove(position);
        let stash_ref = ReferenceName::new("refs/stash")?;
        if entries.is_empty() {
            self.delete_reference(&stash_ref, removed)?;
        } else {
            if index == 0 {
                let next = listed[1].commit;
                self.update_reference(&stash_ref, next, PreviousValue::MustExist(removed))?;
            }
            self.write_atomic(Path::new("logs/refs/stash"), &encode_reflog_chain(&entries))?;
        }
        Ok(removed)
    }

    fn read_stash(&self, index: usize, max_object_size: usize) -> Result<StashData> {
        let entry = self
            .stashes()?
            .get(index)
            .cloned()
            .ok_or_else(|| Error::NotFound(PathBuf::from(format!("stash@{{{index}}}"))))?;
        let commit = self.read_commit(entry.commit, max_object_size)?;
        if !(2..=3).contains(&commit.parents().len()) {
            return Err(Error::InvalidCommit(
                "stash must have two or three parents".into(),
            ));
        }
        let base = self.read_commit(commit.parents()[0], max_object_size)?;
        let index_commit = self.read_commit(commit.parents()[1], max_object_size)?;
        if index_commit.parents() != &commit.parents()[..1] {
            return Err(Error::InvalidCommit(
                "stash index commit must have the base as its only parent".into(),
            ));
        }
        let untracked_tree = commit
            .parents()
            .get(2)
            .map(|id| self.read_commit(*id, max_object_size))
            .transpose()?
            .map(|commit| {
                if commit.parents().is_empty() {
                    Ok(commit.tree())
                } else {
                    Err(Error::InvalidCommit(
                        "stash untracked commit must be a root commit".into(),
                    ))
                }
            })
            .transpose()?;
        Ok(StashData {
            commit: entry.commit,
            base_tree: base.tree(),
            index_tree: index_commit.tree(),
            worktree_tree: commit.tree(),
            untracked_tree,
        })
    }

    fn apply_stash_data(
        &self,
        stash: &StashData,
        options: &StashApplyOptions,
    ) -> Result<StashApplyResult> {
        self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("stash apply requires a working tree".into())
        })?;
        let index = self.read_index()?;
        let current_index_tree = self.write_index_tree(&index)?;
        let current_worktree = self.write_index_tree(&self.snapshot_tracked_worktree(&index)?)?;
        self.reject_stash_untracked_collisions(stash, options.max_object_size)?;
        let restored_index = if options.reinstate_index
            && stash.index_tree != stash.base_tree
            && stash.index_tree != current_index_tree
        {
            Some(self.merge_tree_states_clean(
                stash.base_tree,
                current_index_tree,
                stash.index_tree,
                stash.commit,
                options.max_object_size,
            )?)
        } else {
            None
        };
        let (merged_tree, conflicts) = self.merge_tree_states(
            stash.base_tree,
            current_worktree,
            stash.worktree_tree,
            stash.commit,
            options.max_object_size,
            true,
        )?;
        if conflicts.is_empty() {
            if let Some(tree) = restored_index {
                self.replace_index_with_tree(tree, index.version(), options.max_object_size)?;
            } else {
                self.unstage_stash_changes(
                    current_index_tree,
                    merged_tree,
                    index.version(),
                    options.max_object_size,
                )?;
            }
        }
        if let Some(tree) = stash.untracked_tree {
            self.restore_untracked_tree(tree, index.version(), options.max_object_size)?;
        }
        if conflicts.is_empty() {
            Ok(StashApplyResult::Applied)
        } else {
            Ok(StashApplyResult::Conflicted { paths: conflicts })
        }
    }

    fn snapshot_tracked_worktree(&self, index: &Index) -> Result<Index> {
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("stash requires a working tree".into()))?;
        let mut entries = Vec::new();
        for entry in index.entries() {
            if entry.stage() != 0 {
                return Err(Error::InvalidRepository(format!(
                    "cannot stash unresolved path `{}`",
                    String::from_utf8_lossy(entry.path())
                )));
            }
            if entry.mode() == 0o160_000 {
                entries.push(entry.clone());
                continue;
            }
            let relative = worktree_path(entry.path())?;
            if crate::worktree::has_symlink_leading_path(self.filesystem(), work_tree, &relative)?
            {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let full = work_tree.join(relative);
            let metadata = match self.filesystem().metadata(&full) {
                Ok(metadata) => metadata,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            entries.push(self.snapshot_file(entry.path(), &full, metadata)?);
        }
        Index::new(index.version(), entries)
    }

    fn snapshot_paths(&self, paths: &[Vec<u8>], version: crate::IndexVersion) -> Result<ObjectId> {
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("stash requires a working tree".into()))?;
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            let relative = worktree_path(path)?;
            if crate::worktree::has_symlink_leading_path(self.filesystem(), work_tree, &relative)?
            {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let full = work_tree.join(relative);
            let metadata = self.filesystem().metadata(&full)?;
            entries.push(self.snapshot_file(path, &full, metadata)?);
        }
        self.write_index_tree(&Index::new(version, entries)?)
    }

    fn snapshot_file(
        &self,
        path: &[u8],
        full: &Path,
        metadata: crate::Metadata,
    ) -> Result<IndexEntry> {
        let (contents, mode) = if metadata.is_symlink() {
            (self.filesystem().read_link(full)?, 0o120_000)
        } else if metadata.is_file() {
            (
                self.filesystem().read(full)?,
                if metadata.is_executable() {
                    0o100_755
                } else {
                    0o100_644
                },
            )
        } else {
            return Err(Error::InvalidRepository(format!(
                "stash path `{}` is not a file",
                String::from_utf8_lossy(path)
            )));
        };
        let id = self.write_object(ObjectKind::Blob, &contents)?;
        IndexEntry::new(path.to_vec(), mode, id, StatData::default())
    }

    fn stash_description(&self, head: ObjectId, commit: &crate::Commit) -> String {
        let branch = match self
            .read_reference("HEAD")
            .map(|head| head.target().clone())
        {
            Ok(ReferenceTarget::Symbolic(name)) => name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(name.as_str())
                .to_owned(),
            _ => "(no branch)".to_owned(),
        };
        let hex = head.to_string();
        let subject = commit
            .message()
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        format!(
            "{branch}: {} {}",
            &hex[..7],
            String::from_utf8_lossy(subject)
        )
    }

    fn replace_index_with_tree(
        &self,
        tree: ObjectId,
        version: crate::IndexVersion,
        max_object_size: usize,
    ) -> Result<()> {
        let entries = self
            .flattened_tree(tree, max_object_size)?
            .into_iter()
            .map(|entry| IndexEntry::new(entry.path, entry.raw_mode, entry.id, StatData::default()))
            .collect::<Result<Vec<_>>>()?;
        self.write_index(&Index::new(version, entries)?)
    }

    fn unstage_stash_changes(
        &self,
        current: ObjectId,
        merged: ObjectId,
        version: crate::IndexVersion,
        max_object_size: usize,
    ) -> Result<()> {
        let current = self
            .flattened_tree(current, max_object_size)?
            .into_iter()
            .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
            .collect::<BTreeMap<_, _>>();
        let merged = self
            .flattened_tree(merged, max_object_size)?
            .into_iter()
            .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
            .collect::<BTreeMap<_, _>>();
        let mut entries = current
            .iter()
            .map(|(path, (mode, id))| {
                IndexEntry::new(path.clone(), *mode, *id, StatData::default())
            })
            .collect::<Result<Vec<_>>>()?;
        for (path, (mode, id)) in merged {
            if !current.contains_key(&path) {
                entries.push(IndexEntry::new(path, mode, id, StatData::default())?);
            }
        }
        self.write_index(&Index::new(version, entries)?)
    }

    fn reject_stash_untracked_collisions(
        &self,
        stash: &StashData,
        max_object_size: usize,
    ) -> Result<()> {
        let untracked = self
            .status(&StatusOptions {
                include_untracked: true,
                max_object_size,
            })?
            .entries()
            .iter()
            .filter(|entry| entry.worktree_change() == Some(crate::ChangeKind::Untracked))
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let base = self
            .flattened_tree(stash.base_tree, max_object_size)?
            .into_iter()
            .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
            .collect::<BTreeMap<_, _>>();
        let desired = self
            .flattened_tree(stash.worktree_tree, max_object_size)?
            .into_iter()
            .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
            .collect::<BTreeMap<_, _>>();
        let mut collisions = untracked
            .into_iter()
            .filter(|path| desired.get(path) != base.get(path))
            .collect::<BTreeSet<_>>();
        if let Some(tree) = stash.untracked_tree {
            let work_tree = self.work_tree().ok_or_else(|| {
                Error::InvalidRepository("stash apply requires a working tree".into())
            })?;
            for entry in self.flattened_tree(tree, max_object_size)? {
                let full = work_tree.join(worktree_path(&entry.path)?);
                if self.filesystem().exists(&full)? {
                    collisions.insert(entry.path);
                }
            }
        }
        if collisions.is_empty() {
            Ok(())
        } else {
            Err(Error::CheckoutConflict(
                collisions
                    .into_iter()
                    .map(|path| String::from_utf8_lossy(&path).into_owned())
                    .collect(),
            ))
        }
    }

    fn restore_untracked_tree(
        &self,
        tree: ObjectId,
        _version: crate::IndexVersion,
        max_object_size: usize,
    ) -> Result<()> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("stash apply requires a working tree".into())
        })?;
        let entries = self.flattened_tree(tree, max_object_size)?;
        let mut collisions = Vec::new();
        for entry in &entries {
            let relative = worktree_path(&entry.path)?;
            if crate::worktree::has_symlink_leading_path(self.filesystem(), work_tree, &relative)?
            {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let full = work_tree.join(relative);
            if self.filesystem().exists(&full)? {
                collisions.push(entry.path.clone());
            }
        }
        if !collisions.is_empty() {
            return Err(Error::CheckoutConflict(
                collisions
                    .into_iter()
                    .map(|path| String::from_utf8_lossy(&path).into_owned())
                    .collect(),
            ));
        }
        for entry in entries {
            let relative = worktree_path(&entry.path)?;
            if crate::worktree::has_symlink_leading_path(self.filesystem(), work_tree, &relative)?
            {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let full = work_tree.join(relative);
            if let Some(parent) = full.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            let object = self.read_object(entry.id, max_object_size)?;
            if object.kind() != ObjectKind::Blob {
                return Err(Error::InvalidObject("stash entry is not a blob".into()));
            }
            match entry.mode {
                EntryMode::Link => self.filesystem().create_symlink(&full, object.data())?,
                EntryMode::Blob | EntryMode::BlobExecutable => {
                    self.filesystem().write(&full, object.data())?;
                    self.filesystem()
                        .set_executable(&full, entry.mode == EntryMode::BlobExecutable)?;
                }
                EntryMode::Gitlink | EntryMode::Tree => {
                    return Err(Error::InvalidTree(
                        "untracked stash contains a non-file entry".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn encode_reflog_chain(entries: &[crate::ReflogEntry]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut previous = entries.first().map(crate::ReflogEntry::old_id);
    for entry in entries {
        let old = previous.unwrap_or_else(|| entry.old_id());
        output.extend_from_slice(old.to_string().as_bytes());
        output.push(b' ');
        output.extend_from_slice(entry.new_id().to_string().as_bytes());
        output.push(b' ');
        output.extend_from_slice(entry.committer().encode().as_bytes());
        output.push(b'\t');
        output.extend_from_slice(entry.message());
        output.push(b'\n');
        previous = Some(entry.new_id());
    }
    output
}

fn remove_worktree_file(repository: &Repository, root: &Path, path: &[u8]) -> Result<()> {
    let relative = worktree_path(path)?;
    let full = root.join(&relative);
    match repository.filesystem().remove_file(&full) {
        Ok(()) | Err(Error::NotFound(_)) => {}
        Err(error) => return Err(error),
    }
    let mut parent = relative.parent();
    while let Some(directory) = parent {
        if directory.as_os_str().is_empty() {
            break;
        }
        if repository
            .filesystem()
            .remove_dir(&root.join(directory))
            .is_err()
        {
            break;
        }
        parent = directory.parent();
    }
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn worktree_path(path: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

#[cfg(not(unix))]
fn worktree_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|_| Error::InvalidRepository("non-UTF-8 worktree path".into()))?;
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ChangeKind, FileSystem, HostFileSystem, IndexVersion, InitOptions, MemoryFileSystem, Tree,
        TreeEntry,
    };

    fn fixture() -> (Repository, MemoryFileSystem, Signature, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Stasher", "stash@example.com", 100, 0).unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"base\n")
            .unwrap();
        repository.add("file.txt").unwrap();
        let head = repository
            .commit_index(
                b"base\n",
                &signature,
                &signature,
                &crate::CommitOptions::default(),
            )
            .unwrap();
        (repository, filesystem, signature, head)
    }

    fn checkout_head(repository: &Repository) {
        let head = repository.resolve_reference("HEAD").unwrap();
        let tree = repository.read_commit(head, 4096).unwrap().tree();
        repository
            .checkout_tree(
                tree,
                &CheckoutOptions {
                    force: true,
                    max_object_size: 4096,
                },
            )
            .unwrap();
    }

    #[test]
    fn push_builds_git_topology_and_apply_restores_index_and_untracked() {
        let (repository, filesystem, signature, head) = fixture();
        filesystem
            .write(Path::new("repo/file.txt"), b"staged\n")
            .unwrap();
        repository.add("file.txt").unwrap();
        let staged = repository.read_index().unwrap().entries()[0].id();
        filesystem
            .write(Path::new("repo/file.txt"), b"working\n")
            .unwrap();
        filesystem
            .write(Path::new("repo/new.txt"), b"untracked\n")
            .unwrap();

        let stash = repository
            .stash_push(
                &StashPushOptions {
                    include_untracked: true,
                    message: Some(b"save both layers".to_vec()),
                    max_object_size: 4096,
                },
                &signature,
            )
            .unwrap();
        let commit = repository.read_commit(stash, 4096).unwrap();
        assert_eq!(commit.parents().len(), 3);
        assert_eq!(commit.parents()[0], head);
        assert_eq!(repository.resolve_reference("refs/stash").unwrap(), stash);
        assert_eq!(
            filesystem.read(Path::new("repo/file.txt")).unwrap(),
            b"base\n"
        );
        assert!(!filesystem.exists(Path::new("repo/new.txt")).unwrap());
        assert!(
            repository
                .status(&StatusOptions::default())
                .unwrap()
                .is_clean()
        );
        assert_eq!(
            repository.stashes().unwrap()[0].message(),
            b"save both layers"
        );

        assert_eq!(
            repository
                .stash_apply(
                    0,
                    &StashApplyOptions {
                        reinstate_index: true,
                        max_object_size: 4096,
                    },
                )
                .unwrap(),
            StashApplyResult::Applied
        );
        assert_eq!(
            filesystem.read(Path::new("repo/file.txt")).unwrap(),
            b"working\n"
        );
        assert_eq!(
            filesystem.read(Path::new("repo/new.txt")).unwrap(),
            b"untracked\n"
        );
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), staged);
    }

    #[test]
    fn apply_without_index_unstages_modified_paths() {
        let (repository, filesystem, signature, _) = fixture();
        filesystem
            .write(Path::new("repo/file.txt"), b"staged\n")
            .unwrap();
        repository.add("file.txt").unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"working\n")
            .unwrap();
        repository
            .stash_push(&StashPushOptions::default(), &signature)
            .unwrap();
        repository
            .stash_apply(0, &StashApplyOptions::default())
            .unwrap();
        let status = repository.status(&StatusOptions::default()).unwrap();
        assert_eq!(status.entries().len(), 1);
        assert_eq!(status.entries()[0].index_change(), None);
        assert_eq!(
            status.entries()[0].worktree_change(),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            filesystem.read(Path::new("repo/file.txt")).unwrap(),
            b"working\n"
        );
    }

    #[test]
    fn conflicted_pop_keeps_stash_and_writes_index_stages() {
        let (repository, filesystem, signature, _) = fixture();
        filesystem
            .write(Path::new("repo/file.txt"), b"stashed\n")
            .unwrap();
        repository
            .stash_push(&StashPushOptions::default(), &signature)
            .unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"current\n")
            .unwrap();
        let result = repository
            .stash_pop(0, &StashApplyOptions::default())
            .unwrap();
        assert_eq!(
            result,
            StashApplyResult::Conflicted {
                paths: vec![b"file.txt".to_vec()]
            }
        );
        assert_eq!(repository.stashes().unwrap().len(), 1);
        assert_eq!(repository.read_index().unwrap().entries().len(), 3);
        assert!(
            filesystem
                .read(Path::new("repo/file.txt"))
                .unwrap()
                .starts_with(b"<<<<<<< HEAD\ncurrent\n")
        );
    }

    #[test]
    fn pop_drops_clean_entry_and_drop_rewrites_reflog_chain() {
        let (repository, filesystem, signature, _) = fixture();
        filesystem
            .write(Path::new("repo/file.txt"), b"one\n")
            .unwrap();
        let first = repository
            .stash_push(&StashPushOptions::default(), &signature)
            .unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"two\n")
            .unwrap();
        let second = repository
            .stash_push(&StashPushOptions::default(), &signature)
            .unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"three\n")
            .unwrap();
        let third = repository
            .stash_push(&StashPushOptions::default(), &signature)
            .unwrap();
        assert_eq!(repository.stashes().unwrap().len(), 3);
        assert_eq!(repository.stash_drop(1).unwrap(), second);
        assert_eq!(
            repository
                .stashes()
                .unwrap()
                .iter()
                .map(StashEntry::commit)
                .collect::<Vec<_>>(),
            vec![third, first]
        );
        let log = repository.read_reflog("refs/stash").unwrap();
        assert_eq!(log[1].old_id(), log[0].new_id());

        assert_eq!(
            repository
                .stash_pop(0, &StashApplyOptions::default())
                .unwrap(),
            StashApplyResult::Applied
        );
        assert_eq!(repository.resolve_reference("refs/stash").unwrap(), first);
        checkout_head(&repository);
        assert_eq!(repository.stash_drop(0).unwrap(), first);
        assert!(repository.stashes().unwrap().is_empty());
        assert!(repository.resolve_reference("refs/stash").is_err());
    }

    #[test]
    fn untracked_collision_is_rejected_before_tracked_mutation() {
        let (repository, filesystem, signature, _) = fixture();
        filesystem
            .write(Path::new("repo/saved.txt"), b"saved\n")
            .unwrap();
        repository
            .stash_push(
                &StashPushOptions {
                    include_untracked: true,
                    ..StashPushOptions::default()
                },
                &signature,
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/file.txt"), b"current tracked\n")
            .unwrap();
        filesystem
            .write(Path::new("repo/saved.txt"), b"collision\n")
            .unwrap();
        assert!(matches!(
            repository.stash_apply(0, &StashApplyOptions::default()),
            Err(Error::CheckoutConflict(_))
        ));
        assert_eq!(
            filesystem.read(Path::new("repo/file.txt")).unwrap(),
            b"current tracked\n"
        );
        assert_eq!(
            filesystem.read(Path::new("repo/saved.txt")).unwrap(),
            b"collision\n"
        );
    }

    #[test]
    fn refuses_stash_through_a_symlinked_directory_before_snapshotting() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"top-secret\n").unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Stasher", "stash@example.com", 100, 0).unwrap();
        fs.write(Path::new("repo/base.txt"), b"base\n").unwrap();
        repository.add("base.txt").unwrap();
        repository
            .commit_index(
                b"base\n",
                &signature,
                &signature,
                &crate::CommitOptions::default(),
            )
            .unwrap();
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
            repository.stash_push(&StashPushOptions::default(), &signature),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(outside.join("secret.txt").exists());
        assert!(
            repository.resolve_reference("refs/stash").is_err(),
            "no stash commit may be created from through-link content"
        );
    }

    #[test]
    fn refuses_restoring_untracked_stash_through_a_symlinked_directory() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"secret\n").unwrap();
        let link_tree = repository
            .write_tree(
                &Tree::new(vec![TreeEntry::new(
                    EntryMode::Blob,
                    b"new.txt".to_vec(),
                    blob,
                )
                .unwrap()])
                .unwrap(),
            )
            .unwrap();
        let untracked = repository
            .write_tree(
                &Tree::new(vec![TreeEntry::new(
                    EntryMode::Tree,
                    b"link".to_vec(),
                    link_tree,
                )
                .unwrap()])
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            repository.restore_untracked_tree(untracked, IndexVersion::V2, 4096),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(
            !outside.join("new.txt").exists(),
            "untracked stash content must not be written outside the worktree"
        );
    }
}
