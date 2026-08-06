//! Three-way commit and tree merging over abstract repository storage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{
    CheckoutOptions, CommitBuilder, Error, GraphOptions, Index, IndexEntry, ObjectId, ObjectKind,
    PreviousValue, ReferenceTarget, Repository, Result, Signature, StatData, StatusOptions,
};

/// Fast-forward policy for `Repository::merge`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FastForwardMode {
    #[default]
    Allow,
    Only,
    Never,
}

/// Merge graph, checkout, and commit choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeOptions {
    pub fast_forward: FastForwardMode,
    pub no_commit: bool,
    pub message: Vec<u8>,
    pub graph: GraphOptions,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            fast_forward: FastForwardMode::Allow,
            no_commit: false,
            message: b"Merge commit\n".to_vec(),
            graph: GraphOptions::default(),
        }
    }
}

/// Resulting merge state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeResult {
    UpToDate { head: ObjectId },
    FastForward { old: ObjectId, new: ObjectId },
    Merged { commit: ObjectId },
    Prepared { tree: ObjectId },
    Conflicted { paths: Vec<Vec<u8>> },
}

/// Direction of a single-commit replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayKind {
    CherryPick,
    Revert,
}

/// Cherry-pick/revert parent selection and resource choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayOptions {
    /// One-based parent number for replaying a merge commit.
    pub mainline: Option<usize>,
    pub no_commit: bool,
    pub allow_empty: bool,
    pub max_object_size: usize,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            mainline: None,
            no_commit: false,
            allow_empty: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// Result of cherry-picking or reverting one commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayResult {
    Committed { commit: ObjectId },
    Prepared { tree: ObjectId },
    Conflicted { paths: Vec<Vec<u8>> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MergeEntry {
    mode: u32,
    id: ObjectId,
}

#[derive(Clone, Debug)]
struct Conflict {
    path: Vec<u8>,
    base: Option<MergeEntry>,
    ours: Option<MergeEntry>,
    theirs: Option<MergeEntry>,
    working: MergeEntry,
}

struct TreeMerge {
    resolved: BTreeMap<Vec<u8>, MergeEntry>,
    conflicts: Vec<Conflict>,
}

impl Repository {
    /// Apply or reverse the change introduced by one commit.
    ///
    /// # Errors
    /// Returns an error for dirty or bare repositories, ambiguous merge-commit
    /// parent selection, empty changes unless allowed, corrupt objects, index
    /// or checkout conflicts, or storage failures.
    pub fn replay_commit(
        &self,
        target: ObjectId,
        kind: ReplayKind,
        options: &ReplayOptions,
        committer: &Signature,
    ) -> Result<ReplayResult> {
        if self.work_tree().is_none() {
            return Err(Error::InvalidRepository(
                "commit replay requires a working tree".into(),
            ));
        }
        if self.replay_in_progress()? || self.filesystem().exists(&self.git_path("MERGE_HEAD"))? {
            return Err(Error::InvalidRepository(
                "another merge or replay is already in progress".into(),
            ));
        }
        if !self
            .status(&StatusOptions {
                include_untracked: false,
                max_object_size: options.max_object_size,
            })?
            .is_clean()
        {
            return Err(Error::InvalidRepository(
                "cannot replay with staged or unstaged changes".into(),
            ));
        }
        let ours = self.resolve_reference("HEAD")?;
        let ours_commit = self.read_commit(ours, options.max_object_size)?;
        let picked = self.read_commit(target, options.max_object_size)?;
        let parent_tree = match replay_parent_tree(self, &picked, options)? {
            Some(tree) => tree,
            None => self.empty_tree()?,
        };
        let (base, theirs) = match kind {
            ReplayKind::CherryPick => (parent_tree, picked.tree()),
            ReplayKind::Revert => (picked.tree(), parent_tree),
        };
        let merge_options = MergeOptions {
            graph: GraphOptions {
                max_object_size: options.max_object_size,
                ..GraphOptions::default()
            },
            ..MergeOptions::default()
        };
        let merged = self.merge_trees(base, ours_commit.tree(), theirs, target, &merge_options)?;
        let (tree, paths) = self.materialize_merge(&merged, options.max_object_size)?;
        let message = replay_message(kind, target, &picked, options.mainline);
        if !paths.is_empty() {
            self.write_atomic(Path::new("ORIG_HEAD"), format!("{ours}\n").as_bytes())?;
            self.write_atomic(
                Path::new(replay_head_name(kind)),
                format!("{target}\n").as_bytes(),
            )?;
            self.write_atomic(Path::new("MERGE_MSG"), &message)?;
            return Ok(ReplayResult::Conflicted { paths });
        }
        if tree == ours_commit.tree() && !options.allow_empty {
            return Err(Error::InvalidRepository(
                "replayed commit would be empty".into(),
            ));
        }
        if options.no_commit {
            return Ok(ReplayResult::Prepared { tree });
        }
        let author = match kind {
            ReplayKind::CherryPick => picked.author().clone(),
            ReplayKind::Revert => committer.clone(),
        };
        let id = self.commit_replay(ours, tree, &message, author, committer, kind)?;
        Ok(ReplayResult::Committed { commit: id })
    }

    /// Continue a conflicted cherry-pick or revert after all paths are staged.
    ///
    /// # Errors
    /// Returns an error when no replay is active, stages remain unresolved,
    /// `HEAD` moved, or commit/ref storage fails.
    pub fn continue_replay(
        &self,
        max_object_size: usize,
        committer: &Signature,
    ) -> Result<ObjectId> {
        let (kind, target) = self.read_replay_head()?;
        let ours = self.resolve_reference("HEAD")?;
        if ours != self.read_orig_head()? {
            return Err(Error::ReferenceConflict(
                "HEAD changed during commit replay".into(),
            ));
        }
        let index = self.read_index()?;
        if index.entries().iter().any(|entry| entry.stage() != 0) {
            return Err(Error::InvalidRepository(
                "cannot continue with unresolved replay entries".into(),
            ));
        }
        let tree = self.write_index_tree(&index)?;
        let picked = self.read_commit(target, max_object_size)?;
        let author = match kind {
            ReplayKind::CherryPick => picked.author().clone(),
            ReplayKind::Revert => committer.clone(),
        };
        let message = self.read_git_file("MERGE_MSG")?;
        self.commit_replay(ours, tree, &message, author, committer, kind)
    }

    /// Abort a conflicted cherry-pick or revert.
    ///
    /// # Errors
    /// Returns an error when no replay is active or restoration/storage fails.
    pub fn abort_replay(&self, max_object_size: usize, committer: &Signature) -> Result<()> {
        self.read_replay_head()?;
        let original = self.read_orig_head()?;
        let current = self.resolve_reference("HEAD")?;
        let tree = self.read_commit(original, max_object_size)?.tree();
        self.checkout_tree(
            tree,
            &CheckoutOptions {
                force: true,
                max_object_size,
            },
        )?;
        if current != original {
            self.move_merge_head(current, original, committer, b"reset: moving to ORIG_HEAD")?;
        }
        self.clear_replay_state()
    }

    /// Merge `target` into the current `HEAD`.
    ///
    /// Fast-forwards update the checked-out ref directly. Diverged histories
    /// receive a three-way tree merge. Clean results are committed unless
    /// `no_commit` is set; conflicts are written as index stages 1–3 together
    /// with `MERGE_HEAD`, `MERGE_MSG`, and `ORIG_HEAD`.
    ///
    /// # Errors
    /// Returns an error for a bare or dirty repository, an existing merge,
    /// missing/corrupt commits, graph limits, checkout conflicts, unsupported
    /// file/directory collisions, or ref/storage failures.
    pub fn merge(
        &self,
        target: ObjectId,
        options: &MergeOptions,
        committer: &Signature,
    ) -> Result<MergeResult> {
        if self.work_tree().is_none() {
            return Err(Error::InvalidRepository(
                "merge requires a working tree".into(),
            ));
        }
        if self.filesystem().exists(&self.git_path("MERGE_HEAD"))? {
            return Err(Error::InvalidRepository(
                "a merge is already in progress".into(),
            ));
        }
        if options.message.contains(&0) {
            return Err(Error::InvalidCommit("merge message contains NUL".into()));
        }
        let ours = self.resolve_reference("HEAD")?;
        self.read_commit(ours, options.graph.max_object_size)?;
        self.read_commit(target, options.graph.max_object_size)?;
        if self.is_ancestor(target, ours, &options.graph)? {
            return Ok(MergeResult::UpToDate { head: ours });
        }
        if self.is_ancestor(ours, target, &options.graph)?
            && options.fast_forward != FastForwardMode::Never
        {
            let commit = self.read_commit(target, options.graph.max_object_size)?;
            self.checkout_tree(
                commit.tree(),
                &CheckoutOptions {
                    force: false,
                    max_object_size: options.graph.max_object_size,
                },
            )?;
            self.write_atomic(Path::new("ORIG_HEAD"), format!("{ours}\n").as_bytes())?;
            self.move_merge_head(ours, target, committer, b"merge: Fast-forward")?;
            return Ok(MergeResult::FastForward {
                old: ours,
                new: target,
            });
        }
        if options.fast_forward == FastForwardMode::Only {
            return Err(Error::ReferenceConflict(
                "merge is not a fast-forward".into(),
            ));
        }
        if !self
            .status(&StatusOptions {
                include_untracked: false,
                max_object_size: options.graph.max_object_size,
            })?
            .is_clean()
        {
            return Err(Error::InvalidRepository(
                "cannot merge with staged or unstaged changes".into(),
            ));
        }

        let bases = self.merge_bases(ours, target, &options.graph)?;
        if bases.is_empty() {
            return Err(Error::InvalidRepository(
                "refusing to merge unrelated histories".into(),
            ));
        }
        let base_tree = self.combined_merge_base_tree(&bases, options)?;
        let ours_tree = self
            .read_commit(ours, options.graph.max_object_size)?
            .tree();
        let theirs_tree = self
            .read_commit(target, options.graph.max_object_size)?
            .tree();
        let merged = self.merge_trees(base_tree, ours_tree, theirs_tree, target, options)?;
        let (tree, paths) = self.materialize_merge(&merged, options.graph.max_object_size)?;
        self.write_atomic(Path::new("ORIG_HEAD"), format!("{ours}\n").as_bytes())?;
        self.write_merge_state(target, &options.message)?;
        if !paths.is_empty() {
            return Ok(MergeResult::Conflicted { paths });
        }
        if options.no_commit {
            return Ok(MergeResult::Prepared { tree });
        }
        let commit = self.finish_merge_commit(ours, target, tree, &options.message, committer)?;
        Ok(MergeResult::Merged { commit })
    }

    /// Commit a clean index left by `merge --no-commit` or after conflicts
    /// have been resolved and staged.
    ///
    /// # Errors
    /// Returns an error when no merge is active, unresolved stages remain, the
    /// merge state is corrupt, or object/ref storage fails.
    pub fn continue_merge(
        &self,
        message: Option<&[u8]>,
        committer: &Signature,
    ) -> Result<ObjectId> {
        let target = self.read_merge_head()?;
        let ours = self.resolve_reference("HEAD")?;
        if ours != self.read_orig_head()? {
            return Err(Error::ReferenceConflict(
                "HEAD changed during the merge".into(),
            ));
        }
        let index = self.read_index()?;
        if index.entries().iter().any(|entry| entry.stage() != 0) {
            return Err(Error::InvalidRepository(
                "cannot continue with unresolved merge entries".into(),
            ));
        }
        let tree = self.write_index_tree(&index)?;
        let owned;
        let message = if let Some(message) = message {
            message
        } else {
            owned = self.read_git_file("MERGE_MSG")?;
            owned.as_slice()
        };
        self.finish_merge_commit(ours, target, tree, message, committer)
    }

    /// Abort an in-progress non-fast-forward merge, restoring `HEAD`'s tree
    /// and clearing merge state.
    ///
    /// # Errors
    /// Returns an error when no merge is active or checkout/storage fails.
    pub fn abort_merge(&self, max_object_size: usize, committer: &Signature) -> Result<()> {
        self.read_merge_head()?;
        let original = self.read_orig_head()?;
        let current = self.resolve_reference("HEAD")?;
        let tree = self.read_commit(original, max_object_size)?.tree();
        self.checkout_tree(
            tree,
            &CheckoutOptions {
                force: true,
                max_object_size,
            },
        )?;
        if current != original {
            self.move_merge_head(current, original, committer, b"reset: moving to ORIG_HEAD")?;
        }
        self.clear_merge_state()
    }

    fn combined_merge_base_tree(
        &self,
        bases: &[ObjectId],
        options: &MergeOptions,
    ) -> Result<ObjectId> {
        let mut tree = self
            .read_commit(bases[0], options.graph.max_object_size)?
            .tree();
        for (index, base) in bases.iter().copied().enumerate().skip(1) {
            let other = self
                .read_commit(base, options.graph.max_object_size)?
                .tree();
            let ancestor_tree = if index == 1 {
                let ancestors = self.merge_bases(bases[0], base, &options.graph)?;
                ancestors
                    .first()
                    .map(|id| self.read_commit(*id, options.graph.max_object_size))
                    .transpose()?
                    .map_or(self.empty_tree()?, |commit| commit.tree())
            } else {
                self.empty_tree()?
            };
            let merged = self.merge_trees(ancestor_tree, tree, other, base, options)?;
            tree = self.write_merge_result_tree(&merged)?;
        }
        Ok(tree)
    }

    fn empty_tree(&self) -> Result<ObjectId> {
        self.write_tree(&crate::Tree::new(Vec::new())?)
    }

    fn merge_trees(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        target: ObjectId,
        options: &MergeOptions,
    ) -> Result<TreeMerge> {
        let base = self.tree_map(base, options.graph.max_object_size)?;
        let ours = self.tree_map(ours, options.graph.max_object_size)?;
        let theirs = self.tree_map(theirs, options.graph.max_object_size)?;
        let paths = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut resolved = BTreeMap::new();
        let mut conflicts = Vec::new();
        for path in paths {
            let base_entry = base.get(&path).copied();
            let ours_entry = ours.get(&path).copied();
            let theirs_entry = theirs.get(&path).copied();
            let value = if ours_entry == theirs_entry {
                Some(ours_entry)
            } else if ours_entry == base_entry {
                Some(theirs_entry)
            } else if theirs_entry == base_entry {
                Some(ours_entry)
            } else {
                None
            };
            if let Some(value) = value {
                if let Some(value) = value {
                    resolved.insert(path, value);
                }
            } else {
                let working = self.conflict_working_entry(
                    ours_entry,
                    theirs_entry,
                    target,
                    options.graph.max_object_size,
                )?;
                conflicts.push(Conflict {
                    path,
                    base: base_entry,
                    ours: ours_entry,
                    theirs: theirs_entry,
                    working,
                });
            }
        }
        reject_file_directory_collisions(
            resolved
                .keys()
                .chain(conflicts.iter().map(|conflict| &conflict.path)),
        )?;
        Ok(TreeMerge {
            resolved,
            conflicts,
        })
    }

    fn conflict_working_entry(
        &self,
        ours: Option<MergeEntry>,
        theirs: Option<MergeEntry>,
        target: ObjectId,
        max_size: usize,
    ) -> Result<MergeEntry> {
        if let (Some(ours), Some(theirs)) = (ours, theirs)
            && regular_mode(ours.mode)
            && regular_mode(theirs.mode)
        {
            let ours_data = self.read_blob(ours.id, max_size)?;
            let theirs_data = self.read_blob(theirs.id, max_size)?;
            if !ours_data.contains(&0) && !theirs_data.contains(&0) {
                let mut data = b"<<<<<<< HEAD\n".to_vec();
                append_with_newline(&mut data, &ours_data);
                data.extend_from_slice(b"=======\n");
                append_with_newline(&mut data, &theirs_data);
                data.extend_from_slice(format!(">>>>>>> {target}\n").as_bytes());
                return Ok(MergeEntry {
                    mode: ours.mode,
                    id: self.write_object(ObjectKind::Blob, &data)?,
                });
            }
        }
        ours.or(theirs).ok_or_else(|| {
            Error::InvalidRepository("merge conflict has no materializable side".into())
        })
    }

    fn read_blob(&self, id: ObjectId, max_size: usize) -> Result<Vec<u8>> {
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject("tree entry is not a blob".into()));
        }
        Ok(object.data().to_vec())
    }

    fn tree_map(&self, tree: ObjectId, max_size: usize) -> Result<BTreeMap<Vec<u8>, MergeEntry>> {
        Ok(self
            .flattened_tree(tree, max_size)?
            .into_iter()
            .map(|entry| {
                (
                    entry.path,
                    MergeEntry {
                        mode: entry.raw_mode,
                        id: entry.id,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>())
    }

    fn materialize_merge(
        &self,
        merged: &TreeMerge,
        max_object_size: usize,
    ) -> Result<(ObjectId, Vec<Vec<u8>>)> {
        let mut material = merged.resolved.clone();
        for conflict in &merged.conflicts {
            material.insert(conflict.path.clone(), conflict.working);
        }
        let material_tree = self.write_entry_map_tree(&material)?;
        self.checkout_tree(
            material_tree,
            &CheckoutOptions {
                force: false,
                max_object_size,
            },
        )?;
        if merged.conflicts.is_empty() {
            return Ok((material_tree, Vec::new()));
        }
        let conflict_paths = merged
            .conflicts
            .iter()
            .map(|conflict| conflict.path.clone())
            .collect::<BTreeSet<_>>();
        let mut entries = self
            .read_index()?
            .entries()
            .iter()
            .filter(|entry| !conflict_paths.contains(entry.path()))
            .cloned()
            .collect::<Vec<_>>();
        for conflict in &merged.conflicts {
            for (stage, value) in [(1, conflict.base), (2, conflict.ours), (3, conflict.theirs)] {
                if let Some(value) = value {
                    entries.push(IndexEntry::with_stage(
                        conflict.path.clone(),
                        value.mode,
                        value.id,
                        StatData::default(),
                        stage,
                    )?);
                }
            }
        }
        self.write_index(&Index::new(self.read_index()?.version(), entries)?)?;
        Ok((
            material_tree,
            conflict_paths.into_iter().collect::<Vec<_>>(),
        ))
    }

    fn write_merge_result_tree(&self, merged: &TreeMerge) -> Result<ObjectId> {
        let mut entries = merged.resolved.clone();
        for conflict in &merged.conflicts {
            entries.insert(conflict.path.clone(), conflict.working);
        }
        self.write_entry_map_tree(&entries)
    }

    fn write_entry_map_tree(&self, values: &BTreeMap<Vec<u8>, MergeEntry>) -> Result<ObjectId> {
        let entries = values
            .iter()
            .map(|(path, value)| {
                IndexEntry::new(path.clone(), value.mode, value.id, StatData::default())
            })
            .collect::<Result<Vec<_>>>()?;
        self.write_index_tree(&Index::new(self.read_index()?.version(), entries)?)
    }

    fn write_merge_state(&self, target: ObjectId, message: &[u8]) -> Result<()> {
        if message.contains(&0) {
            return Err(Error::InvalidCommit("merge message contains NUL".into()));
        }
        self.write_atomic(Path::new("MERGE_HEAD"), format!("{target}\n").as_bytes())?;
        self.write_atomic(Path::new("MERGE_MSG"), message)
    }

    fn read_merge_head(&self) -> Result<ObjectId> {
        let data = self.read_git_file("MERGE_HEAD")?;
        let value = data.strip_suffix(b"\n").unwrap_or(&data);
        std::str::from_utf8(value)
            .map_err(|_| Error::InvalidRepository("MERGE_HEAD is not ASCII".into()))?
            .parse()
            .map_err(|_| Error::InvalidRepository("MERGE_HEAD is invalid".into()))
    }

    fn read_orig_head(&self) -> Result<ObjectId> {
        let data = self.read_git_file("ORIG_HEAD")?;
        let value = data.strip_suffix(b"\n").unwrap_or(&data);
        std::str::from_utf8(value)
            .map_err(|_| Error::InvalidRepository("ORIG_HEAD is not ASCII".into()))?
            .parse()
            .map_err(|_| Error::InvalidRepository("ORIG_HEAD is invalid".into()))
    }

    fn finish_merge_commit(
        &self,
        ours: ObjectId,
        target: ObjectId,
        tree: ObjectId,
        message: &[u8],
        committer: &Signature,
    ) -> Result<ObjectId> {
        let commit = CommitBuilder::new(tree, committer.clone(), committer.clone())
            .parent(ours)
            .parent(target)
            .message(message.to_vec())
            .build();
        let id = self.write_commit(&commit)?;
        self.move_merge_head(ours, id, committer, b"merge: made merge commit")?;
        self.clear_merge_state()?;
        Ok(id)
    }

    fn move_merge_head(
        &self,
        old: ObjectId,
        new: ObjectId,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        match self.read_reference("HEAD")?.target() {
            ReferenceTarget::Symbolic(branch) => {
                self.update_reference_with_reflog(
                    branch,
                    new,
                    PreviousValue::MustExist(old),
                    committer,
                    message,
                )?;
                self.append_reflog("HEAD", old, new, committer, message)
            }
            ReferenceTarget::Direct(_) => {
                self.write_atomic(Path::new("HEAD"), format!("{new}\n").as_bytes())?;
                self.append_reflog("HEAD", old, new, committer, message)
            }
        }
    }

    fn clear_merge_state(&self) -> Result<()> {
        for name in ["MERGE_HEAD", "MERGE_MSG"] {
            let path = self.git_path(name);
            match self.filesystem().remove_file(&path) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn replay_in_progress(&self) -> Result<bool> {
        Ok(self
            .filesystem()
            .exists(&self.git_path("CHERRY_PICK_HEAD"))?
            || self.filesystem().exists(&self.git_path("REVERT_HEAD"))?)
    }

    fn read_replay_head(&self) -> Result<(ReplayKind, ObjectId)> {
        for (kind, name) in [
            (ReplayKind::CherryPick, "CHERRY_PICK_HEAD"),
            (ReplayKind::Revert, "REVERT_HEAD"),
        ] {
            match self.read_git_file(name) {
                Ok(data) => {
                    let value = data.strip_suffix(b"\n").unwrap_or(&data);
                    let id = std::str::from_utf8(value)
                        .map_err(|_| Error::InvalidRepository("replay HEAD is not ASCII".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidRepository("replay HEAD is invalid".into()))?;
                    return Ok((kind, id));
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::InvalidRepository(
            "no cherry-pick or revert is in progress".into(),
        ))
    }

    fn commit_replay(
        &self,
        ours: ObjectId,
        tree: ObjectId,
        message: &[u8],
        author: Signature,
        committer: &Signature,
        kind: ReplayKind,
    ) -> Result<ObjectId> {
        let commit = CommitBuilder::new(tree, author, committer.clone())
            .parent(ours)
            .message(message.to_vec())
            .build();
        let id = self.write_commit(&commit)?;
        let action = match kind {
            ReplayKind::CherryPick => b"cherry-pick: applied commit".as_slice(),
            ReplayKind::Revert => b"revert: reverted commit".as_slice(),
        };
        self.move_merge_head(ours, id, committer, action)?;
        self.clear_replay_state()?;
        Ok(id)
    }

    fn clear_replay_state(&self) -> Result<()> {
        for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG"] {
            let path = self.git_path(name);
            match self.filesystem().remove_file(&path) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn replay_parent_tree(
    repository: &Repository,
    commit: &crate::Commit,
    options: &ReplayOptions,
) -> Result<Option<ObjectId>> {
    if commit.parents().is_empty() {
        if options.mainline.is_some() {
            return Err(Error::InvalidCommit(
                "mainline parent specified for a root commit".into(),
            ));
        }
        return Ok(None);
    }
    let index = if commit.parents().len() == 1 {
        if options.mainline.is_some() {
            return Err(Error::InvalidCommit(
                "mainline parent specified for a non-merge commit".into(),
            ));
        }
        0
    } else {
        options
            .mainline
            .and_then(|value| value.checked_sub(1))
            .filter(|index| *index < commit.parents().len())
            .ok_or_else(|| Error::InvalidCommit("merge commit requires a valid mainline".into()))?
    };
    Ok(Some(
        repository
            .read_commit(commit.parents()[index], options.max_object_size)?
            .tree(),
    ))
}

fn replay_head_name(kind: ReplayKind) -> &'static str {
    match kind {
        ReplayKind::CherryPick => "CHERRY_PICK_HEAD",
        ReplayKind::Revert => "REVERT_HEAD",
    }
}

fn replay_message(
    kind: ReplayKind,
    target: ObjectId,
    commit: &crate::Commit,
    mainline: Option<usize>,
) -> Vec<u8> {
    match kind {
        ReplayKind::CherryPick => commit.message().to_vec(),
        ReplayKind::Revert => {
            let subject = commit
                .message()
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let mut message = b"Revert \"".to_vec();
            message.extend_from_slice(subject);
            message.extend_from_slice(b"\"\n\nThis reverts commit ");
            message.extend_from_slice(target.to_string().as_bytes());
            if let Some(mainline) = mainline {
                message.extend_from_slice(
                    format!(", reversing changes made to its parent {mainline}").as_bytes(),
                );
            }
            message.extend_from_slice(b".\n");
            message
        }
    }
}

fn reject_file_directory_collisions<'a>(paths: impl Iterator<Item = &'a Vec<u8>>) -> Result<()> {
    let paths = paths.collect::<BTreeSet<_>>();
    for path in &paths {
        for slash in path
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'/').then_some(index))
        {
            if paths.contains(&path[..slash].to_vec()) {
                return Err(Error::InvalidRepository(format!(
                    "file/directory merge conflict at `{}`",
                    String::from_utf8_lossy(&path[..slash])
                )));
            }
        }
    }
    Ok(())
}

const fn regular_mode(mode: u32) -> bool {
    matches!(mode, 0o100_644 | 0o100_755)
}

fn append_with_newline(output: &mut Vec<u8>, contents: &[u8]) {
    output.extend_from_slice(contents);
    if !contents.ends_with(b"\n") {
        output.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        FastForwardMode, MergeOptions, MergeResult, ReplayKind, ReplayOptions, ReplayResult,
    };
    use crate::{
        CheckoutOptions, CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem,
        ObjectKind, PreviousValue, ReferenceName, Repository, Signature, Tree, TreeEntry,
    };

    #[test]
    fn fast_forwards_and_detects_up_to_date_heads() {
        let (repository, filesystem, signature) = repository();
        let base = commit(&repository, &[], &[(&b"file"[..], b"base\n")], 1);
        set_main(&repository, base);
        checkout(&repository, base);
        let tip = commit(&repository, &[base], &[(&b"file"[..], b"tip\n")], 2);

        assert_eq!(
            repository
                .merge(tip, &MergeOptions::default(), &signature)
                .unwrap(),
            MergeResult::FastForward {
                old: base,
                new: tip
            }
        );
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"tip\n");
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), tip);
        assert!(matches!(
            repository
                .merge(base, &MergeOptions::default(), &signature)
                .unwrap(),
            MergeResult::UpToDate { head } if head == tip
        ));
    }

    #[test]
    fn creates_a_clean_two_parent_merge_and_supports_no_commit() {
        let (repository, filesystem, signature) = repository();
        let base = commit(
            &repository,
            &[],
            &[
                (&b"left"[..], b"base-left\n"),
                (&b"right"[..], b"base-right\n"),
            ],
            1,
        );
        let ours = commit(
            &repository,
            &[base],
            &[(&b"left"[..], b"ours\n"), (&b"right"[..], b"base-right\n")],
            2,
        );
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"left"[..], b"base-left\n"), (&b"right"[..], b"theirs\n")],
            3,
        );
        set_main(&repository, ours);
        checkout(&repository, ours);

        let result = repository
            .merge(
                theirs,
                &MergeOptions {
                    fast_forward: FastForwardMode::Never,
                    no_commit: true,
                    message: b"Merge topic\n".to_vec(),
                    ..MergeOptions::default()
                },
                &signature,
            )
            .unwrap();
        assert!(matches!(result, MergeResult::Prepared { .. }));
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), ours);
        assert_eq!(filesystem.read(Path::new("repo/left")).unwrap(), b"ours\n");
        assert_eq!(
            filesystem.read(Path::new("repo/right")).unwrap(),
            b"theirs\n"
        );
        let merged = repository.continue_merge(None, &signature).unwrap();
        let commit = repository.read_commit(merged, 4096).unwrap();
        assert_eq!(commit.parents(), [ours, theirs]);
        assert_eq!(commit.message(), b"Merge topic\n");
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/MERGE_HEAD"))
                .unwrap()
        );
    }

    #[test]
    fn writes_conflict_stages_continues_after_resolution_and_aborts() {
        let (repository, filesystem, signature) = repository();
        let base = commit(&repository, &[], &[(&b"file"[..], b"base\n")], 1);
        let ours = commit(&repository, &[base], &[(&b"file"[..], b"ours\n")], 2);
        let theirs = commit(&repository, &[base], &[(&b"file"[..], b"theirs\n")], 3);
        set_main(&repository, ours);
        checkout(&repository, ours);

        let result = repository
            .merge(theirs, &MergeOptions::default(), &signature)
            .unwrap();
        assert_eq!(
            result,
            MergeResult::Conflicted {
                paths: vec![b"file".to_vec()]
            }
        );
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .map(crate::IndexEntry::stage)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let working = filesystem.read(Path::new("repo/file")).unwrap();
        assert!(working.starts_with(b"<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> "));
        assert!(repository.continue_merge(None, &signature).is_err());

        filesystem
            .write(Path::new("repo/file"), b"resolved\n")
            .unwrap();
        repository.add("file").unwrap();
        let merged = repository
            .continue_merge(Some(b"resolved merge\n"), &signature)
            .unwrap();
        assert_eq!(
            repository.read_commit(merged, 4096).unwrap().parents(),
            [ours, theirs]
        );

        repository
            .reset(
                ours,
                &crate::ResetOptions {
                    mode: crate::ResetMode::Hard,
                    ..crate::ResetOptions::default()
                },
                &signature,
            )
            .unwrap();
        repository
            .merge(theirs, &MergeOptions::default(), &signature)
            .unwrap();
        repository.abort_merge(4096, &signature).unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"ours\n");
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .all(|entry| entry.stage() == 0)
        );
    }

    #[test]
    fn cherry_picks_with_original_author_and_reverts_the_change() {
        let (repository, filesystem, signature) = repository();
        let base = commit(
            &repository,
            &[],
            &[(&b"file"[..], b"base\n"), (&b"other"[..], b"base\n")],
            1,
        );
        let picked = commit(
            &repository,
            &[base],
            &[(&b"file"[..], b"picked\n"), (&b"other"[..], b"base\n")],
            2,
        );
        let ours = commit(
            &repository,
            &[base],
            &[(&b"file"[..], b"base\n"), (&b"other"[..], b"ours\n")],
            3,
        );
        set_main(&repository, ours);
        checkout(&repository, ours);

        let replayed = repository
            .replay_commit(
                picked,
                ReplayKind::CherryPick,
                &ReplayOptions::default(),
                &signature,
            )
            .unwrap();
        let ReplayResult::Committed { commit: cherry } = replayed else {
            panic!("expected committed cherry-pick");
        };
        let cherry_commit = repository.read_commit(cherry, 4096).unwrap();
        assert_eq!(cherry_commit.parents(), [ours]);
        assert_eq!(cherry_commit.author().timestamp(), 2);
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"picked\n"
        );
        assert_eq!(filesystem.read(Path::new("repo/other")).unwrap(), b"ours\n");

        let reverted = repository
            .replay_commit(
                picked,
                ReplayKind::Revert,
                &ReplayOptions::default(),
                &signature,
            )
            .unwrap();
        let ReplayResult::Committed { commit: revert } = reverted else {
            panic!("expected committed revert");
        };
        let revert_commit = repository.read_commit(revert, 4096).unwrap();
        assert_eq!(revert_commit.parents(), [cherry]);
        assert!(revert_commit.message().starts_with(b"Revert \"commit\""));
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"base\n");
        assert_eq!(filesystem.read(Path::new("repo/other")).unwrap(), b"ours\n");
    }

    #[test]
    fn cherry_pick_conflicts_can_continue_or_abort() {
        let (repository, filesystem, signature) = repository();
        let base = commit(&repository, &[], &[(&b"file"[..], b"base\n")], 1);
        let ours = commit(&repository, &[base], &[(&b"file"[..], b"ours\n")], 2);
        let picked = commit(&repository, &[base], &[(&b"file"[..], b"picked\n")], 3);
        set_main(&repository, ours);
        checkout(&repository, ours);

        assert!(matches!(
            repository
                .replay_commit(
                    picked,
                    ReplayKind::CherryPick,
                    &ReplayOptions::default(),
                    &signature,
                )
                .unwrap(),
            ReplayResult::Conflicted { .. }
        ));
        assert!(
            filesystem
                .exists(Path::new("repo/.git/CHERRY_PICK_HEAD"))
                .unwrap()
        );
        assert!(repository.continue_replay(4096, &signature).is_err());
        filesystem
            .write(Path::new("repo/file"), b"resolved\n")
            .unwrap();
        repository.add("file").unwrap();
        let replayed = repository.continue_replay(4096, &signature).unwrap();
        assert_eq!(
            repository.read_commit(replayed, 4096).unwrap().parents(),
            [ours]
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/CHERRY_PICK_HEAD"))
                .unwrap()
        );

        repository
            .reset(
                ours,
                &crate::ResetOptions {
                    mode: crate::ResetMode::Hard,
                    ..crate::ResetOptions::default()
                },
                &signature,
            )
            .unwrap();
        repository
            .replay_commit(
                picked,
                ReplayKind::CherryPick,
                &ReplayOptions::default(),
                &signature,
            )
            .unwrap();
        repository.abort_replay(4096, &signature).unwrap();
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"ours\n");
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .all(|entry| entry.stage() == 0)
        );
    }

    fn repository() -> (Repository, MemoryFileSystem, Signature) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Merge", "merge@example.com", 10, 0).unwrap();
        (repository, filesystem, signature)
    }

    fn set_main(repository: &Repository, id: crate::ObjectId) {
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                id,
                PreviousValue::Any,
            )
            .unwrap();
    }

    fn checkout(repository: &Repository, id: crate::ObjectId) {
        let tree = repository.read_commit(id, 4096).unwrap().tree();
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

    fn commit(
        repository: &Repository,
        parents: &[crate::ObjectId],
        files: &[(&[u8], &[u8])],
        timestamp: i64,
    ) -> crate::ObjectId {
        let mut entries = Vec::new();
        for (path, contents) in files {
            let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
            entries.push(TreeEntry::new(EntryMode::Blob, path.to_vec(), blob).unwrap());
        }
        let tree = repository.write_tree(&Tree::new(entries).unwrap()).unwrap();
        let identity = Signature::new("Merge", "merge@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository
            .write_commit(&builder.message(b"commit\n".to_vec()).build())
            .unwrap()
    }
}
