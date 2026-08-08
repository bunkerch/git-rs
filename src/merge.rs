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
    pub max_text_merge_lines: usize,
    pub max_diff_trace_cells: usize,
    pub max_rename_comparisons: usize,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            fast_forward: FastForwardMode::Allow,
            no_commit: false,
            message: b"Merge commit\n".to_vec(),
            graph: GraphOptions::default(),
            max_text_merge_lines: 1_000_000,
            max_diff_trace_cells: 10_000_000,
            max_rename_comparisons: 1_000_000,
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

/// Choices for a non-checkout tree merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTreeOptions {
    /// Use this commit or tree as the merge base instead of discovering bases.
    pub merge_base: Option<ObjectId>,
    /// Treat the empty tree as the base when the commits have no common history.
    pub allow_unrelated_histories: bool,
    pub graph: GraphOptions,
    pub max_text_merge_lines: usize,
    pub max_diff_trace_cells: usize,
    pub max_rename_comparisons: usize,
}

impl Default for MergeTreeOptions {
    fn default() -> Self {
        Self {
            merge_base: None,
            allow_unrelated_histories: false,
            graph: GraphOptions::default(),
            max_text_merge_lines: 1_000_000,
            max_diff_trace_cells: 10_000_000,
            max_rename_comparisons: 1_000_000,
        }
    }
}

/// One stage of a conflicted path returned by [`Repository::merge_tree`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTreeStage {
    /// Path associated with this stage; rename conflicts can use different paths.
    pub path: Vec<u8>,
    pub stage: u8,
    pub mode: u32,
    pub id: ObjectId,
}

/// Structured conflict information from a non-checkout tree merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTreeConflict {
    pub path: Vec<u8>,
    /// Present stages in Git index order: base (1), ours (2), theirs (3).
    pub stages: Vec<MergeTreeStage>,
}

/// Tree object and conflicts produced without changing repository state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTreeResult {
    pub tree: ObjectId,
    pub conflicts: Vec<MergeTreeConflict>,
}

impl MergeTreeResult {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MergeEntry {
    mode: u32,
    id: ObjectId,
}

#[derive(Clone, Debug)]
struct Conflict {
    path: Vec<u8>,
    stages: Vec<ConflictStage>,
    working: Vec<(Vec<u8>, MergeEntry)>,
}

#[derive(Clone, Debug)]
struct ConflictStage {
    path: Vec<u8>,
    stage: u8,
    entry: MergeEntry,
}

struct TreeMerge {
    resolved: BTreeMap<Vec<u8>, MergeEntry>,
    conflicts: Vec<Conflict>,
}

impl Repository {
    /// Merge two commits into a new tree without reading or changing `HEAD`,
    /// refs, the index, or the worktree.
    ///
    /// The resulting tree is written to object storage. Conflicted regular
    /// files contain marker text in that tree, while `conflicts` retains the
    /// original base/ours/theirs stages as typed object identities.
    ///
    /// # Errors
    /// Returns an error for non-commit inputs without an explicit base,
    /// unrelated histories unless allowed, invalid object kinds, corrupt
    /// graphs or trees, resource-limit violations, or storage failures.
    pub fn merge_tree(
        &self,
        ours: ObjectId,
        theirs: ObjectId,
        options: &MergeTreeOptions,
    ) -> Result<MergeTreeResult> {
        let (base_tree, ours_tree, theirs_tree) = if let Some(base) = options.merge_base {
            (
                self.merge_input_tree(base, options.graph.max_object_size)?,
                self.merge_input_tree(ours, options.graph.max_object_size)?,
                self.merge_input_tree(theirs, options.graph.max_object_size)?,
            )
        } else {
            let ours_commit = self.read_commit(ours, options.graph.max_object_size)?;
            let theirs_commit = self.read_commit(theirs, options.graph.max_object_size)?;
            let bases = self.merge_bases(ours, theirs, &options.graph)?;
            let base_tree = if bases.is_empty() {
                if !options.allow_unrelated_histories {
                    return Err(Error::InvalidRepository(
                        "refusing to merge unrelated histories".into(),
                    ));
                }
                self.empty_tree()?
            } else {
                self.combined_merge_base_tree(
                    &bases,
                    &MergeOptions {
                        graph: options.graph.clone(),
                        max_text_merge_lines: options.max_text_merge_lines,
                        max_diff_trace_cells: options.max_diff_trace_cells,
                        max_rename_comparisons: options.max_rename_comparisons,
                        ..MergeOptions::default()
                    },
                )?
            };
            (base_tree, ours_commit.tree(), theirs_commit.tree())
        };
        let merged = self.merge_trees(
            base_tree,
            ours_tree,
            theirs_tree,
            Some(ours),
            theirs,
            &MergeOptions {
                graph: options.graph.clone(),
                max_text_merge_lines: options.max_text_merge_lines,
                max_diff_trace_cells: options.max_diff_trace_cells,
                max_rename_comparisons: options.max_rename_comparisons,
                ..MergeOptions::default()
            },
        )?;
        let tree = self.write_merge_result_tree(&merged)?;
        let conflicts = merged
            .conflicts
            .iter()
            .map(|conflict| MergeTreeConflict {
                path: conflict.path.clone(),
                stages: conflict
                    .stages
                    .iter()
                    .map(|stage| MergeTreeStage {
                        path: stage.path.clone(),
                        stage: stage.stage,
                        mode: stage.entry.mode,
                        id: stage.entry.id,
                    })
                    .collect(),
            })
            .collect();
        Ok(MergeTreeResult { tree, conflicts })
    }

    fn merge_input_tree(&self, id: ObjectId, max_size: usize) -> Result<ObjectId> {
        let object = self.read_object(id, max_size)?;
        match object.kind() {
            ObjectKind::Commit => Ok(crate::Commit::parse(object.data())?.tree()),
            ObjectKind::Tree => Ok(id),
            kind => Err(Error::InvalidObject(format!(
                "merge-tree input {id} is {kind:?}, expected commit or tree"
            ))),
        }
    }

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
        let merged = self.merge_trees(
            base,
            ours_commit.tree(),
            theirs,
            None,
            target,
            &merge_options,
        )?;
        let (tree, paths) = self.materialize_merge(&merged, options.max_object_size, false)?;
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
            return Err(Error::EmptyReplay);
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
        let merged = self.merge_trees(base_tree, ours_tree, theirs_tree, None, target, options)?;
        let (tree, paths) =
            self.materialize_merge(&merged, options.graph.max_object_size, false)?;
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
            let merged = self.merge_trees(ancestor_tree, tree, other, None, base, options)?;
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
        ours_label: Option<ObjectId>,
        target: ObjectId,
        options: &MergeOptions,
    ) -> Result<TreeMerge> {
        let mut base = self.tree_map(base, options.graph.max_object_size)?;
        let mut ours = self.tree_map(ours, options.graph.max_object_size)?;
        let mut theirs = self.tree_map(theirs, options.graph.max_object_size)?;
        let mut conflicts = align_exact_renames(
            &mut base,
            &mut ours,
            &mut theirs,
            options.max_rename_comparisons,
        )?;
        let paths = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut resolved = BTreeMap::new();
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
                let (working, content_conflict) = self.conflict_working_entry(
                    base_entry,
                    ours_entry,
                    theirs_entry,
                    ours_label,
                    target,
                    options,
                )?;
                if content_conflict {
                    conflicts.push(Conflict {
                        stages: [(1, base_entry), (2, ours_entry), (3, theirs_entry)]
                            .into_iter()
                            .filter_map(|(stage, entry)| {
                                entry.map(|entry| ConflictStage {
                                    path: path.clone(),
                                    stage,
                                    entry,
                                })
                            })
                            .collect(),
                        working: vec![(path.clone(), working)],
                        path,
                    });
                } else {
                    resolved.insert(path, working);
                }
            }
        }
        reject_file_directory_collisions(
            resolved.keys().chain(
                conflicts
                    .iter()
                    .flat_map(|conflict| conflict.working.iter().map(|(path, _)| path)),
            ),
        )?;
        Ok(TreeMerge {
            resolved,
            conflicts,
        })
    }

    fn conflict_working_entry(
        &self,
        base: Option<MergeEntry>,
        ours: Option<MergeEntry>,
        theirs: Option<MergeEntry>,
        ours_label: Option<ObjectId>,
        target: ObjectId,
        options: &MergeOptions,
    ) -> Result<(MergeEntry, bool)> {
        if let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs)
            && regular_mode(base.mode)
            && regular_mode(ours.mode)
            && regular_mode(theirs.mode)
        {
            let base_data = self.read_blob(base.id, options.graph.max_object_size)?;
            let ours_data = self.read_blob(ours.id, options.graph.max_object_size)?;
            let theirs_data = self.read_blob(theirs.id, options.graph.max_object_size)?;
            if !base_data.contains(&0) && !ours_data.contains(&0) && !theirs_data.contains(&0) {
                let ours_label = ours_label.map(|id| id.to_string());
                let (data, conflicted) = crate::diff::merge_text(
                    &base_data,
                    &ours_data,
                    &theirs_data,
                    ours_label.as_deref().unwrap_or("HEAD").as_bytes(),
                    target.to_string().as_bytes(),
                    options.max_text_merge_lines,
                    options.max_diff_trace_cells,
                )?;
                let mode = merge_scalar(base.mode, ours.mode, theirs.mode).unwrap_or(ours.mode);
                return Ok((
                    MergeEntry {
                        mode,
                        id: self.write_object(ObjectKind::Blob, &data)?,
                    },
                    conflicted || merge_scalar(base.mode, ours.mode, theirs.mode).is_none(),
                ));
            }
        }
        ours.or(theirs).map(|entry| (entry, true)).ok_or_else(|| {
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

    pub(crate) fn merge_tree_states(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        target: ObjectId,
        max_object_size: usize,
        force_checkout: bool,
    ) -> Result<(ObjectId, Vec<Vec<u8>>)> {
        let options = MergeOptions {
            graph: GraphOptions {
                max_object_size,
                ..GraphOptions::default()
            },
            ..MergeOptions::default()
        };
        let merged = self.merge_trees(base, ours, theirs, None, target, &options)?;
        self.materialize_merge(&merged, max_object_size, force_checkout)
    }

    pub(crate) fn merge_tree_states_clean(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        target: ObjectId,
        max_object_size: usize,
    ) -> Result<ObjectId> {
        let options = MergeOptions {
            graph: GraphOptions {
                max_object_size,
                ..GraphOptions::default()
            },
            ..MergeOptions::default()
        };
        let merged = self.merge_trees(base, ours, theirs, None, target, &options)?;
        if !merged.conflicts.is_empty() {
            return Err(Error::CheckoutConflict(
                merged
                    .conflicts
                    .iter()
                    .map(|conflict| String::from_utf8_lossy(&conflict.path).into_owned())
                    .collect(),
            ));
        }
        self.write_merge_result_tree(&merged)
    }

    fn materialize_merge(
        &self,
        merged: &TreeMerge,
        max_object_size: usize,
        force_checkout: bool,
    ) -> Result<(ObjectId, Vec<Vec<u8>>)> {
        let mut material = merged.resolved.clone();
        for conflict in &merged.conflicts {
            material.extend(conflict.working.iter().cloned());
        }
        let material_tree = self.write_entry_map_tree(&material)?;
        self.checkout_tree(
            material_tree,
            &CheckoutOptions {
                force: force_checkout,
                max_object_size,
            },
        )?;
        if merged.conflicts.is_empty() {
            return Ok((material_tree, Vec::new()));
        }
        let conflict_paths = merged
            .conflicts
            .iter()
            .flat_map(|conflict| {
                conflict
                    .stages
                    .iter()
                    .map(|stage| stage.path.clone())
                    .chain(conflict.working.iter().map(|(path, _)| path.clone()))
            })
            .collect::<BTreeSet<_>>();
        let mut entries = self
            .read_index()?
            .entries()
            .iter()
            .filter(|entry| !conflict_paths.contains(entry.path()))
            .cloned()
            .collect::<Vec<_>>();
        for conflict in &merged.conflicts {
            for stage in &conflict.stages {
                entries.push(IndexEntry::with_stage(
                    stage.path.clone(),
                    stage.entry.mode,
                    stage.entry.id,
                    StatData::default(),
                    stage.stage,
                )?);
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
            entries.extend(conflict.working.iter().cloned());
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
        self.write_index_tree(&Index::new(crate::IndexVersion::V2, entries)?)
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
            if paths
                .iter()
                .any(|existing| existing.as_slice() == &path[..slash])
            {
                return Err(Error::InvalidRepository(format!(
                    "file/directory merge conflict at `{}`",
                    String::from_utf8_lossy(&path[..slash])
                )));
            }
        }
    }
    Ok(())
}

fn align_exact_renames(
    base: &mut BTreeMap<Vec<u8>, MergeEntry>,
    ours: &mut BTreeMap<Vec<u8>, MergeEntry>,
    theirs: &mut BTreeMap<Vec<u8>, MergeEntry>,
    max_comparisons: usize,
) -> Result<Vec<Conflict>> {
    let ours_renames = exact_renames(base, ours, max_comparisons)?;
    let theirs_renames = exact_renames(base, theirs, max_comparisons)?;
    let old_paths = ours_renames
        .keys()
        .chain(theirs_renames.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut conflicts = Vec::new();
    for old in old_paths {
        let base_entry = base.get(&old).copied().ok_or_else(|| {
            Error::InvalidRepository("rename source disappeared during merge".into())
        })?;
        match (ours_renames.get(&old), theirs_renames.get(&old)) {
            (Some(ours_new), Some(theirs_new)) if ours_new == theirs_new => {
                move_entry(base, &old, ours_new)?;
            }
            (Some(ours_new), Some(theirs_new)) => {
                base.remove(&old);
                let ours_entry = ours.remove(ours_new).ok_or_else(|| {
                    Error::InvalidRepository("ours rename destination disappeared".into())
                })?;
                let theirs_entry = theirs.remove(theirs_new).ok_or_else(|| {
                    Error::InvalidRepository("theirs rename destination disappeared".into())
                })?;
                conflicts.push(Conflict {
                    path: old.clone(),
                    stages: vec![
                        ConflictStage {
                            path: old,
                            stage: 1,
                            entry: base_entry,
                        },
                        ConflictStage {
                            path: ours_new.clone(),
                            stage: 2,
                            entry: ours_entry,
                        },
                        ConflictStage {
                            path: theirs_new.clone(),
                            stage: 3,
                            entry: theirs_entry,
                        },
                    ],
                    working: vec![
                        (ours_new.clone(), ours_entry),
                        (theirs_new.clone(), theirs_entry),
                    ],
                });
            }
            (Some(new), None) => align_one_sided_rename(
                base,
                ours,
                theirs,
                &old,
                new,
                base_entry,
                2,
                &mut conflicts,
            )?,
            (None, Some(new)) => align_one_sided_rename(
                base,
                theirs,
                ours,
                &old,
                new,
                base_entry,
                3,
                &mut conflicts,
            )?,
            (None, None) => unreachable!("path came from a rename map"),
        }
    }
    Ok(conflicts)
}

#[allow(clippy::too_many_arguments)]
fn align_one_sided_rename(
    base: &mut BTreeMap<Vec<u8>, MergeEntry>,
    renamed_side: &mut BTreeMap<Vec<u8>, MergeEntry>,
    other_side: &mut BTreeMap<Vec<u8>, MergeEntry>,
    old: &[u8],
    new: &[u8],
    base_entry: MergeEntry,
    renamed_stage: u8,
    conflicts: &mut Vec<Conflict>,
) -> Result<()> {
    let renamed_entry = renamed_side.get(new).copied().ok_or_else(|| {
        Error::InvalidRepository("rename destination disappeared during merge".into())
    })?;
    if let Some(other_entry) = other_side.remove(old) {
        move_entry(base, old, new)?;
        if other_side.insert(new.to_vec(), other_entry).is_some() {
            return Err(Error::InvalidRepository(format!(
                "rename destination `{}` collides with another path",
                String::from_utf8_lossy(new)
            )));
        }
        return Ok(());
    }
    base.remove(old);
    renamed_side.remove(new);
    conflicts.push(Conflict {
        path: new.to_vec(),
        stages: vec![
            ConflictStage {
                path: new.to_vec(),
                stage: 1,
                entry: base_entry,
            },
            ConflictStage {
                path: new.to_vec(),
                stage: renamed_stage,
                entry: renamed_entry,
            },
        ],
        working: vec![(new.to_vec(), renamed_entry)],
    });
    Ok(())
}

fn move_entry(values: &mut BTreeMap<Vec<u8>, MergeEntry>, old: &[u8], new: &[u8]) -> Result<()> {
    let value = values
        .remove(old)
        .ok_or_else(|| Error::InvalidRepository("rename source disappeared during merge".into()))?;
    if values.insert(new.to_vec(), value).is_some() {
        return Err(Error::InvalidRepository(format!(
            "rename destination `{}` collides with another path",
            String::from_utf8_lossy(new)
        )));
    }
    Ok(())
}

fn exact_renames(
    base: &BTreeMap<Vec<u8>, MergeEntry>,
    side: &BTreeMap<Vec<u8>, MergeEntry>,
    max_comparisons: usize,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut additions = BTreeMap::<MergeEntry, Vec<&Vec<u8>>>::new();
    for (path, entry) in side {
        if !base.contains_key(path) {
            additions.entry(*entry).or_default().push(path);
        }
    }
    let mut used = BTreeSet::new();
    let mut renames = BTreeMap::new();
    let mut comparisons = 0usize;
    for (old, entry) in base {
        if side.contains_key(old) {
            continue;
        }
        let Some(candidates) = additions.get(entry) else {
            continue;
        };
        let mut ranked = Vec::new();
        for candidate in candidates
            .iter()
            .copied()
            .filter(|path| !used.contains(*path))
        {
            comparisons = comparisons.checked_add(1).ok_or_else(|| {
                Error::InvalidRepository("rename comparison count overflow".into())
            })?;
            if comparisons > max_comparisons {
                return Err(Error::InvalidRepository(format!(
                    "exact rename detection exceeds {max_comparisons} comparisons"
                )));
            }
            ranked.push((path_similarity(old, candidate), candidate));
        }
        ranked.sort_unstable_by(|left, right| right.cmp(left));
        if let Some((_, new)) = ranked.first() {
            used.insert((*new).clone());
            renames.insert(old.clone(), (*new).clone());
        }
    }
    Ok(renames)
}

fn path_similarity(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
        + left
            .iter()
            .rev()
            .zip(right.iter().rev())
            .take_while(|(left, right)| left == right)
            .count()
}

fn merge_scalar<T: Copy + Eq>(base: T, ours: T, theirs: T) -> Option<T> {
    if ours == theirs {
        Some(ours)
    } else if ours == base {
        Some(theirs)
    } else if theirs == base {
        Some(ours)
    } else {
        None
    }
}

const fn regular_mode(mode: u32) -> bool {
    matches!(mode, 0o100_644 | 0o100_755)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        FastForwardMode, MergeOptions, MergeResult, MergeTreeOptions, ReplayKind, ReplayOptions,
        ReplayResult,
    };
    use crate::{
        CheckoutOptions, CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem,
        ObjectKind, PreviousValue, ReferenceName, ReferenceTarget, Repository, Signature, Tree,
        TreeEntry,
    };

    #[test]
    fn merge_tree_writes_result_and_structured_conflicts_without_mutating_state() {
        let (repository, filesystem, _) = repository();
        let base = commit(
            &repository,
            &[],
            &[(&b"a"[..], b"base a\n"), (&b"b"[..], b"base b\n")],
            1,
        );
        let ours = commit(
            &repository,
            &[base],
            &[(&b"a"[..], b"ours a\n"), (&b"b"[..], b"base b\n")],
            2,
        );
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"a"[..], b"base a\n"), (&b"b"[..], b"theirs b\n")],
            3,
        );
        set_main(&repository, ours);
        checkout(&repository, ours);
        let head_before = filesystem.read(Path::new("repo/.git/HEAD")).unwrap();
        let index_before = filesystem.read(Path::new("repo/.git/index")).unwrap();
        let worktree_before = filesystem.read(Path::new("repo/a")).unwrap();

        let clean = repository
            .merge_tree(ours, theirs, &MergeTreeOptions::default())
            .unwrap();
        assert!(clean.is_clean());
        let entries = repository.flattened_tree(clean.tree, 4096).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            repository.read_object(entries[0].id, 4096).unwrap().data(),
            b"ours a\n"
        );
        assert_eq!(
            repository.read_object(entries[1].id, 4096).unwrap().data(),
            b"theirs b\n"
        );

        let conflicting = commit(
            &repository,
            &[base],
            &[(&b"a"[..], b"theirs a\n"), (&b"b"[..], b"base b\n")],
            4,
        );
        let result = repository
            .merge_tree(ours, conflicting, &MergeTreeOptions::default())
            .unwrap();
        assert!(!result.is_clean());
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].path, b"a");
        assert_eq!(
            result.conflicts[0]
                .stages
                .iter()
                .map(|stage| stage.stage)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let marker = repository
            .flattened_tree(result.tree, 4096)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == b"a")
            .unwrap();
        assert!(
            repository
                .read_object(marker.id, 4096)
                .unwrap()
                .data()
                .starts_with(b"<<<<<<< ")
        );
        assert_eq!(
            filesystem.read(Path::new("repo/.git/HEAD")).unwrap(),
            head_before
        );
        assert_eq!(
            filesystem.read(Path::new("repo/.git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            filesystem.read(Path::new("repo/a")).unwrap(),
            worktree_before
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/MERGE_HEAD"))
                .unwrap()
        );
    }

    #[test]
    fn merge_tree_runs_in_a_bare_repository_and_accepts_an_explicit_tree_base() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo.git",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let base = commit(&repository, &[], &[(&b"file"[..], b"base\n")], 1);
        let ours = commit(&repository, &[base], &[(&b"file"[..], b"ours\n")], 2);
        let base_tree = repository.read_commit(base, 4096).unwrap().tree();
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"file"[..], b"base\n"), (&b"other"[..], b"new\n")],
            3,
        );
        let theirs_tree = repository.read_commit(theirs, 4096).unwrap().tree();
        let result = repository
            .merge_tree(
                ours,
                theirs_tree,
                &MergeTreeOptions {
                    merge_base: Some(base_tree),
                    ..MergeTreeOptions::default()
                },
            )
            .unwrap();
        assert!(result.is_clean());
        assert_eq!(
            repository.flattened_tree(result.tree, 4096).unwrap().len(),
            2
        );
    }
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
    fn merges_disjoint_edits_within_the_same_text_file() {
        let (repository, filesystem, signature) = repository();
        let base = commit(&repository, &[], &[(&b"file"[..], b"one\ntwo\nthree\n")], 1);
        let ours = commit(
            &repository,
            &[base],
            &[(&b"file"[..], b"ONE\ntwo\nthree\n")],
            2,
        );
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"file"[..], b"one\ntwo\nTHREE\n")],
            3,
        );
        let tree_result = repository
            .merge_tree(ours, theirs, &MergeTreeOptions::default())
            .unwrap();
        assert!(tree_result.is_clean());
        set_main(&repository, ours);
        checkout(&repository, ours);
        let result = repository
            .merge(
                theirs,
                &MergeOptions {
                    no_commit: true,
                    ..MergeOptions::default()
                },
                &signature,
            )
            .unwrap();
        assert_eq!(
            result,
            MergeResult::Prepared {
                tree: tree_result.tree
            }
        );
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"ONE\ntwo\nTHREE\n"
        );
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
    fn aligns_exact_renames_and_preserves_path_specific_conflict_stages() {
        let (repository, _, _) = repository();
        let base = commit(
            &repository,
            &[],
            &[(&b"old"[..], b"line one\nline two\n")],
            1,
        );
        let ours_rename = commit(
            &repository,
            &[base],
            &[(&b"new"[..], b"line one\nline two\n")],
            2,
        );
        let theirs_edit = commit(
            &repository,
            &[base],
            &[(&b"old"[..], b"LINE ONE\nline two\n")],
            3,
        );
        let result = repository
            .merge_tree(ours_rename, theirs_edit, &MergeTreeOptions::default())
            .unwrap();
        assert!(result.is_clean());
        let entries = repository.flattened_tree(result.tree, 4096).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, b"new");
        assert_eq!(
            repository.read_object(entries[0].id, 4096).unwrap().data(),
            b"LINE ONE\nline two\n"
        );

        let ours = commit(
            &repository,
            &[base],
            &[(&b"ours-name"[..], b"line one\nline two\n")],
            4,
        );
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"theirs-name"[..], b"line one\nline two\n")],
            5,
        );
        let result = repository
            .merge_tree(ours, theirs, &MergeTreeOptions::default())
            .unwrap();
        assert!(!result.is_clean());
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].path, b"old");
        assert_eq!(
            result.conflicts[0]
                .stages
                .iter()
                .map(|stage| (stage.stage, stage.path.as_slice()))
                .collect::<Vec<_>>(),
            [
                (1, &b"old"[..]),
                (2, &b"ours-name"[..]),
                (3, &b"theirs-name"[..])
            ]
        );
        assert_eq!(
            repository
                .flattened_tree(result.tree, 4096)
                .unwrap()
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            [b"ours-name".to_vec(), b"theirs-name".to_vec()]
        );

        let deleted = commit(&repository, &[base], &[], 6);
        let result = repository
            .merge_tree(ours_rename, deleted, &MergeTreeOptions::default())
            .unwrap();
        assert!(!result.is_clean());
        assert_eq!(result.conflicts[0].path, b"new");
        assert_eq!(
            result.conflicts[0]
                .stages
                .iter()
                .map(|stage| (stage.stage, stage.path.as_slice()))
                .collect::<Vec<_>>(),
            [(1, &b"new"[..]), (2, &b"new"[..])]
        );
    }

    #[test]
    fn materializes_divergent_rename_stages_at_their_git_paths() {
        let (repository, filesystem, signature) = repository();
        let base = commit(&repository, &[], &[(&b"old"[..], b"content\n")], 1);
        let ours = commit(
            &repository,
            &[base],
            &[(&b"ours-name"[..], b"content\n")],
            2,
        );
        let theirs = commit(
            &repository,
            &[base],
            &[(&b"theirs-name"[..], b"content\n")],
            3,
        );
        set_main(&repository, ours);
        checkout(&repository, ours);
        let result = repository
            .merge(theirs, &MergeOptions::default(), &signature)
            .unwrap();
        assert!(matches!(result, MergeResult::Conflicted { .. }));
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .map(|entry| (entry.stage(), entry.path()))
                .collect::<Vec<_>>(),
            [
                (1, &b"old"[..]),
                (2, &b"ours-name"[..]),
                (3, &b"theirs-name"[..])
            ]
        );
        assert!(filesystem.exists(Path::new("repo/ours-name")).unwrap());
        assert!(filesystem.exists(Path::new("repo/theirs-name")).unwrap());
        assert!(!filesystem.exists(Path::new("repo/old")).unwrap());
    }

    #[test]
    fn bounds_exact_rename_candidate_comparisons() {
        let (repository, _, _) = repository();
        let base = commit(
            &repository,
            &[],
            &[(&b"a"[..], b"same\n"), (&b"b"[..], b"same\n")],
            1,
        );
        let renamed = commit(
            &repository,
            &[base],
            &[(&b"c"[..], b"same\n"), (&b"d"[..], b"same\n")],
            2,
        );
        assert!(
            repository
                .merge_tree(
                    renamed,
                    base,
                    &MergeTreeOptions {
                        max_rename_comparisons: 0,
                        ..MergeTreeOptions::default()
                    },
                )
                .is_err()
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

    #[test]
    fn ff_only_rejects_divergent_history() {
        let (repository, _, signature) = repository();
        let base = commit(&repository, &[], &[(&b"f"[..], b"base\n")], 1);
        set_main(&repository, base);
        checkout(&repository, base);

        let ours = commit(&repository, &[base], &[(&b"f"[..], b"ours\n")], 2);
        set_main(&repository, ours);
        checkout(&repository, ours);
        let theirs = commit(&repository, &[base], &[(&b"f"[..], b"theirs\n")], 3);

        let result = repository.merge(
            theirs,
            &MergeOptions {
                fast_forward: FastForwardMode::Only,
                ..MergeOptions::default()
            },
            &signature,
        );
        assert!(result.is_err(), "ff-only should reject divergent history");
    }

    #[test]
    fn modify_delete_conflict_is_detected() {
        let (repository, _, _) = repository();
        let base = commit(&repository, &[], &[(&b"shared"[..], b"base\n")], 1);
        set_main(&repository, base);
        checkout(&repository, base);

        let ours = commit(&repository, &[base], &[(&b"shared"[..], b"ours\n")], 2);
        // theirs deletes "shared"
        let theirs = commit(&repository, &[base], &[], 3);

        let result = repository.merge_tree(
            ours,
            theirs,
            &MergeTreeOptions {
                merge_base: Some(base),
                ..MergeTreeOptions::default()
            },
        );
        assert!(
            result.is_ok(),
            "modify/delete merge should produce conflicts: {result:?}"
        );
    }

    #[test]
    fn file_directory_conflict_is_detected() {
        let (repository, _, _) = repository();
        let base = commit(&repository, &[], &[], 1);
        set_main(&repository, base);
        checkout(&repository, base);

        // ours creates a file "path"
        let ours = commit(&repository, &[base], &[(&b"path"[..], b"file content\n")], 2);
        // theirs creates a directory with "path/file"
        let theirs_blob = repository.write_object(ObjectKind::Blob, b"nested\n").unwrap();
        let sub_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), theirs_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("Merge", "merge@example.com", 3, 0).unwrap();
        let theirs_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"path".to_vec(), sub_tree).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let theirs = repository
            .write_commit(
                &CommitBuilder::new(theirs_tree, identity.clone(), identity)
                    .parent(base)
                    .message(b"theirs\n".to_vec())
                    .build(),
            )
            .unwrap();

        let result = repository.merge_tree(
            ours,
            theirs,
            &MergeTreeOptions {
                merge_base: Some(base),
                ..MergeTreeOptions::default()
            },
        );
        assert!(
            result.is_err(),
            "file/directory merge should be detected as error: {result:?}"
        );
    }

    #[test]
    fn unborn_merge_creates_first_commit() {
        let (repository, _, signature) = repository();
        // Repository has no commits yet (unborn)
        let head_ref = repository.read_reference("HEAD").unwrap();
        assert!(
            matches!(head_ref.target(), ReferenceTarget::Symbolic(_)),
            "fresh repo should have symbolic HEAD"
        );

        // Create a commit that would be the merge target
        let blob_id = repository.write_object(ObjectKind::Blob, b"first\n").unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let first = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .message(b"first\n".to_vec())
                    .build(),
            )
            .unwrap();

        // Set the branch target so HEAD resolves
        set_main(&repository, first);

        assert_eq!(
            repository.resolve_reference("HEAD").unwrap(),
            first,
            "unborn repo should resolve HEAD after setting branch"
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
