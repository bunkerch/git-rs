//! Worktree-to-index operations over the abstract filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::{
    EntryMode, Error, FileStat, FileSystem, IgnoreMatcher, Index, IndexEntry, ObjectId, ObjectKind,
    Repository, Result, StatData, Tree, TreeEntry,
};

#[derive(Clone, Debug)]
pub struct CheckoutOptions {
    pub force: bool,
    pub max_object_size: usize,
}

/// Controls worktree-to-index staging.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AddOptions {
    /// Include ignored untracked paths, equivalent to `git add --force`.
    pub force: bool,
}

/// Selection, mutation, and resource policy for an atomic multi-path add.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddTransactionOptions {
    pub force: bool,
    /// Update tracked paths but do not add untracked paths.
    pub update_only: bool,
    /// Stage tracked paths missing from the worktree as deletions.
    pub include_removals: bool,
    /// Record new paths with the empty-blob ID and intent-to-add flag.
    pub intent_to_add: bool,
    /// Override the executable bit for regular files.
    pub executable: Option<bool>,
    /// Validate and report without writing objects or the index.
    pub dry_run: bool,
    pub max_file_size: usize,
    pub max_paths: usize,
}

impl Default for AddTransactionOptions {
    fn default() -> Self {
        Self {
            force: false,
            update_only: false,
            include_removals: true,
            intent_to_add: false,
            executable: None,
            dry_run: false,
            max_file_size: 1024 * 1024 * 1024,
            max_paths: 1_000_000,
        }
    }
}

/// Paths staged, removed, or skipped by an add transaction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AddReport {
    pub staged: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
    pub ignored: Vec<Vec<u8>>,
}

/// Safety, selection, and mutation policy for tracked-path removal.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoveOptions {
    /// Remove only from the index and leave worktree files in place.
    pub cached: bool,
    /// Override staged and local modification checks.
    pub force: bool,
    /// Permit a selected directory prefix to remove all tracked descendants.
    pub recursive: bool,
    /// Succeed when a requested path matches no tracked entry.
    pub ignore_unmatched: bool,
    /// Include entries marked `skip-worktree`.
    pub include_sparse: bool,
    /// Validate and return selected paths without changing index or worktree.
    pub dry_run: bool,
    pub max_object_size: usize,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            cached: false,
            force: false,
            recursive: false,
            ignore_unmatched: false,
            include_sparse: false,
            dry_run: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// Collision, sparse-index, and mutation policy for a tracked move.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MoveOptions {
    /// Replace an existing regular file/symlink destination and its index entry.
    pub force: bool,
    /// Include source entries marked `skip-worktree`.
    pub include_sparse: bool,
    /// Perform full validation without changing the worktree or index.
    pub dry_run: bool,
}

impl Default for CheckoutOptions {
    fn default() -> Self {
        Self {
            force: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Add a file, symlink, directory tree, or deletion to the index.
    ///
    /// Directories are traversed recursively. Entries below the selected path
    /// are replaced as one atomic index transaction, so removed worktree files
    /// are staged as deletions. Git metadata is never traversed.
    ///
    /// # Errors
    /// Returns an error for bare repositories, unsafe paths, unsupported file
    /// types, filesystem failures, object-write failures, or index contention.
    pub fn add(&self, path: impl AsRef<Path>) -> Result<usize> {
        self.add_with_options(path, &AddOptions::default())
    }

    /// Add with explicit ignore policy.
    ///
    /// # Errors
    /// Returns [`Error::IgnoredPath`] for an explicitly selected ignored file
    /// unless `force` is enabled, plus the errors documented by [`Self::add`].
    pub fn add_with_options(&self, path: impl AsRef<Path>, options: &AddOptions) -> Result<usize> {
        let relative = normalize_relative(path.as_ref())?;
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("cannot add from a bare repository".into()))?;
        if has_symlink_leading_path(self.filesystem(), work_tree, &relative)? {
            return Err(Error::BeyondSymbolicLink(relative));
        }
        let existing = self.read_index()?;
        let tracked = existing
            .entries()
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<std::collections::BTreeSet<_>>();
        let mut ignores = self.ignore_matcher()?;
        ignores.add_worktree_patterns(self, work_tree, b"")?;
        let mut ignore_base = PathBuf::new();
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                if let Component::Normal(component) = component {
                    ignore_base.push(component);
                    ignores.add_worktree_patterns(self, work_tree, &index_path(&ignore_base)?)?;
                }
            }
        }
        let mut added = Vec::new();
        let target = work_tree.join(&relative);
        match self.filesystem().metadata(&target) {
            Ok(metadata) => {
                let selected = index_path(&relative)?;
                if !options.force
                    && !metadata.is_dir()
                    && !tracked.contains(&selected)
                    && ignores.is_ignored(&selected, false)
                {
                    return Err(Error::IgnoredPath(relative));
                }
                self.collect_entries(
                    work_tree,
                    &relative,
                    &tracked,
                    &mut ignores,
                    options.force,
                    &mut added,
                )?;
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let prefix = index_path(&relative)?;
        let mut entries = existing.entries().to_vec();
        entries.retain(|entry| !path_is_selected(entry.path(), &prefix));
        let count = added.len();
        entries.extend(added);
        self.write_index(&Index::new(existing.version(), entries)?)?;
        Ok(count)
    }

    /// Atomically stage multiple literal files/directories and their deletions.
    ///
    /// All paths, ignores, metadata, and file contents are preflighted before
    /// object or index publication. Selections are literal repository-relative
    /// paths; a selected directory recursively covers its descendants.
    ///
    /// # Errors
    /// Returns an error for an empty or excessive selection, unmatched paths,
    /// unsafe paths, ignored explicit files, oversized inputs, bare repositories,
    /// unsupported file types, or object/index storage failures.
    #[allow(clippy::too_many_lines)]
    pub fn add_paths<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: &AddTransactionOptions,
    ) -> Result<AddReport> {
        if paths.is_empty() || paths.len() > options.max_paths {
            return Err(Error::InvalidRepository("invalid add path count".into()));
        }
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("cannot add from a bare repository".into()))?;
        let existing = self.read_index()?;
        let tracked = existing
            .entries()
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<std::collections::BTreeSet<_>>();
        let mut selections = Vec::with_capacity(paths.len());
        let mut pending = BTreeMap::<Vec<u8>, PendingAdd>::new();
        let mut ignored = std::collections::BTreeSet::new();
        for path in paths {
            let relative = normalize_relative(path.as_ref())?;
            if has_symlink_leading_path(self.filesystem(), work_tree, &relative)? {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let prefix = index_path(&relative)?;
            let tracked_match = tracked
                .iter()
                .any(|candidate| path_is_selected(candidate, &prefix));
            let target = work_tree.join(&relative);
            let metadata = match self.filesystem().metadata(&target) {
                Ok(metadata) => Some(metadata),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            if metadata.is_none() && !tracked_match {
                return Err(Error::InvalidPath(relative));
            }
            let mut matcher = self.ignore_matcher()?;
            matcher.add_worktree_patterns(self, work_tree, b"")?;
            self.collect_add_plan(
                work_tree,
                &relative,
                &tracked,
                &mut matcher,
                options,
                true,
                &mut pending,
                &mut ignored,
            )?;
            selections.push(prefix);
        }

        let removed = existing
            .entries()
            .iter()
            .filter(|entry| {
                selections
                    .iter()
                    .any(|prefix| path_is_selected(entry.path(), prefix))
                    && !pending.contains_key(entry.path())
                    && options.include_removals
            })
            .map(|entry| entry.path().to_vec())
            .collect::<std::collections::BTreeSet<_>>();
        if pending
            .len()
            .saturating_add(removed.len())
            .saturating_add(ignored.len())
            > options.max_paths
        {
            return Err(Error::InvalidRepository(
                "add result exceeds path limit".into(),
            ));
        }
        let staged = pending.keys().cloned().collect::<Vec<_>>();
        if !options.dry_run {
            let mut entries = existing.entries().to_vec();
            entries.retain(|entry| {
                !pending.contains_key(entry.path()) && !removed.contains(entry.path())
            });
            for (path, plan) in pending {
                let is_new = !tracked.contains(&path);
                let (id, stat) = if options.intent_to_add && is_new {
                    (
                        ObjectId::compute(ObjectKind::Blob, b""),
                        StatData::default(),
                    )
                } else {
                    (
                        self.write_object(ObjectKind::Blob, &plan.contents)?,
                        plan.stat,
                    )
                };
                entries.push(
                    IndexEntry::new(path, plan.mode, id, stat)?
                        .with_intent_to_add(options.intent_to_add && is_new),
                );
            }
            let version = if options.intent_to_add && existing.version() == crate::IndexVersion::V2
            {
                crate::IndexVersion::V3
            } else {
                existing.version()
            };
            self.write_index(&Index::new(version, entries)?)?;
        }
        Ok(AddReport {
            staged,
            removed: removed.iter().cloned().collect(),
            ignored: ignored.into_iter().collect(),
        })
    }

    /// Remove one or more literal tracked paths from the index and worktree.
    ///
    /// A path naming a tracked directory prefix requires `recursive`. All
    /// selections and content-safety checks finish before the first mutation.
    /// This method intentionally accepts literal repository paths rather than
    /// CLI pathspec syntax; callers can perform their own pattern expansion.
    ///
    /// # Errors
    /// Returns an error for no paths, unsafe/unmatched/recursive selections,
    /// sparse entries without opt-in, local or staged changes without force,
    /// object bounds, index contention, or filesystem failures.
    #[allow(clippy::too_many_lines)]
    pub fn remove<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: &RemoveOptions,
    ) -> Result<Vec<Vec<u8>>> {
        if paths.is_empty() {
            return Err(Error::InvalidPath(PathBuf::new()));
        }
        let work_tree = if options.cached {
            self.work_tree()
        } else {
            Some(self.work_tree().ok_or_else(|| {
                Error::InvalidRepository("worktree removal requires a non-bare repository".into())
            })?)
        };
        let index = self.read_index()?;
        let requested = paths
            .iter()
            .map(|path| normalize_relative(path.as_ref()).and_then(|path| index_path(&path)))
            .collect::<Result<Vec<_>>>()?;
        // Index-only (`cached`) removals never touch the filesystem, so the
        // symlink guard applies only to worktree removals.
        if !options.cached {
            if let Some(root) = work_tree {
                for path in &requested {
                    let relative = worktree_path(path)?;
                    if has_symlink_leading_path(self.filesystem(), root, &relative)? {
                        return Err(Error::BeyondSymbolicLink(relative));
                    }
                }
            }
        }
        let mut selected = BTreeMap::<Vec<u8>, Vec<&IndexEntry>>::new();
        for prefix in &requested {
            let matches = index
                .entries()
                .iter()
                .filter(|entry| path_is_selected(entry.path(), prefix))
                .collect::<Vec<_>>();
            if matches.is_empty() {
                if !options.ignore_unmatched {
                    return Err(Error::NotFound(worktree_path(prefix)?));
                }
                continue;
            }
            let recursive_match = matches.iter().any(|entry| entry.path() != prefix);
            if recursive_match && !options.recursive {
                return Err(Error::InvalidRepository(format!(
                    "not removing `{}` recursively without recursive=true",
                    String::from_utf8_lossy(prefix)
                )));
            }
            for entry in matches {
                if entry.skip_worktree() && !options.include_sparse {
                    return Err(Error::InvalidRepository(format!(
                        "path `{}` is outside the sparse worktree",
                        String::from_utf8_lossy(entry.path())
                    )));
                }
                selected
                    .entry(entry.path().to_vec())
                    .or_default()
                    .push(entry);
            }
        }
        if selected.is_empty() {
            return Ok(Vec::new());
        }

        let head = match self.resolve_reference("HEAD") {
            Ok(id) => self
                .flattened_tree(
                    self.read_commit(id, options.max_object_size)?.tree(),
                    options.max_object_size,
                )?
                .into_iter()
                .map(|entry| (entry.path, (entry.raw_mode, entry.id)))
                .collect::<BTreeMap<_, _>>(),
            Err(Error::NotFound(_)) => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        if !options.force {
            let mut conflicts = Vec::new();
            for (path, entries) in &selected {
                let conflict_stages = entries
                    .iter()
                    .map(|entry| entry.stage())
                    .collect::<std::collections::BTreeSet<_>>();
                if conflict_stages.iter().any(|stage| *stage != 0) {
                    continue;
                }
                let entry = entries[0];
                let staged = head
                    .get(path)
                    .is_none_or(|(mode, id)| *mode != entry.mode() || *id != entry.id());
                let local = match work_tree {
                    Some(root) => {
                        let full = root.join(worktree_path(path)?);
                        match self.filesystem().metadata(&full) {
                            Ok(metadata) if metadata.is_dir() && entry.mode() != 0o160_000 => None,
                            Ok(_) => Some(!self.worktree_matches(entry, &full)?),
                            Err(Error::NotFound(_)) => None,
                            Err(error) => return Err(error),
                        }
                    }
                    None => Some(false),
                };
                let Some(local) = local else { continue };
                let unsafe_removal = if options.cached {
                    local && staged && !entry.intent_to_add()
                } else {
                    local || staged
                };
                if unsafe_removal {
                    conflicts.push(String::from_utf8_lossy(path).into_owned());
                }
            }
            if !conflicts.is_empty() {
                return Err(Error::CheckoutConflict(conflicts));
            }
        }

        let removed = selected.keys().cloned().collect::<Vec<_>>();
        if options.dry_run {
            return Ok(removed);
        }
        let mut mutation_error = None;
        if !options.cached {
            let root = work_tree.ok_or_else(|| {
                Error::InvalidRepository("worktree removal requires a non-bare repository".into())
            })?;
            for path in &removed {
                let relative = worktree_path(path)?;
                if has_symlink_leading_path(self.filesystem(), root, &relative)? {
                    return Err(Error::BeyondSymbolicLink(relative));
                }
            }
            for (removed_from_worktree, path) in removed.iter().enumerate() {
                let full = root.join(worktree_path(path)?);
                let entry = selected[path][0];
                let result = match self.filesystem().metadata(&full) {
                    Ok(metadata) if metadata.is_dir() && entry.mode() == 0o160_000 => {
                        remove_worktree_tree(self, &full, options.force)
                    }
                    Ok(metadata) if metadata.is_dir() => Ok(()),
                    Ok(_) => self.filesystem().remove_file(&full),
                    Err(Error::NotFound(_)) => Ok(()),
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    if removed_from_worktree == 0 {
                        return Err(error);
                    }
                    mutation_error = Some(error);
                    break;
                }
                if let Err(error) = self.prune_empty_parents(root, worktree_path(path)?.parent()) {
                    mutation_error = Some(error);
                    break;
                }
            }
        }
        let remaining = index
            .entries()
            .iter()
            .filter(|entry| !selected.contains_key(entry.path()))
            .cloned()
            .collect::<Vec<_>>();
        self.write_index(&Index::new(index.version(), remaining)?)?;
        if let Some(error) = mutation_error {
            return Err(error);
        }
        Ok(removed)
    }

    /// Move one literal tracked file, gitlink, or directory prefix.
    ///
    /// Staged object IDs, stat data, stages, and extended index flags are
    /// preserved under the destination path. Local worktree modifications and
    /// untracked files inside a moved directory travel with it. The destination
    /// is interpreted literally (not as the CLI's multi-source directory form).
    ///
    /// # Errors
    /// Returns an error for unsafe paths, a missing/untracked source, unresolved
    /// stages, sparse entries without opt-in, self-nesting, file/directory or
    /// index collisions, unavailable destination parents, transfer failure, or
    /// index publication failure.
    #[allow(clippy::too_many_lines)]
    pub fn move_path(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        options: &MoveOptions,
    ) -> Result<usize> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("moving paths requires a non-bare repository".into())
        })?;
        let source = normalize_relative(source.as_ref())?;
        let destination = normalize_relative(destination.as_ref())?;
        if has_symlink_leading_path(self.filesystem(), work_tree, &source)? {
            return Err(Error::BeyondSymbolicLink(source));
        }
        // Reject a destination reached through any symlinked directory. Git's
        // `git mv` writes through a depth-1 symlinked destination parent, but
        // git-rs deliberately guards all leading components: any symlinked
        // intermediate can redirect the rename outside the repository root,
        // which the triage's confinement boundary must prevent.
        if has_symlink_leading_path(self.filesystem(), work_tree, &destination)? {
            return Err(Error::BeyondSymbolicLink(destination));
        }
        let source_index = index_path(&source)?;
        let destination_index = index_path(&destination)?;
        if source_index.is_empty()
            || destination_index.is_empty()
            || source_index == destination_index
        {
            return Err(Error::InvalidPath(destination));
        }
        let index = self.read_index()?;
        let selected = index
            .entries()
            .iter()
            .filter(|entry| path_is_selected(entry.path(), &source_index))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(Error::NotFound(source));
        }
        if selected.iter().any(|entry| entry.stage() != 0) {
            return Err(Error::InvalidRepository(format!(
                "cannot move unresolved path `{}`",
                String::from_utf8_lossy(&source_index)
            )));
        }
        if selected.iter().any(|entry| entry.skip_worktree()) && !options.include_sparse {
            return Err(Error::InvalidRepository(format!(
                "path `{}` is outside the sparse worktree",
                String::from_utf8_lossy(&source_index)
            )));
        }
        let exact = selected.iter().find(|entry| entry.path() == source_index);
        let source_path = work_tree.join(&source);
        let source_metadata = match self.filesystem().metadata(&source_path) {
            Ok(metadata) => Some(metadata),
            Err(Error::NotFound(_))
                if options.include_sparse && selected.iter().all(|entry| entry.skip_worktree()) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let directory = source_metadata.is_some_and(crate::Metadata::is_dir)
            || (source_metadata.is_none() && exact.is_none());
        if source_metadata.is_some_and(crate::Metadata::is_dir)
            && exact.is_some_and(|entry| entry.mode() != 0o160_000)
        {
            return Err(Error::InvalidRepository(
                "tracked file source was replaced by a directory".into(),
            ));
        }
        if directory
            && destination_index.starts_with(&source_index)
            && destination_index.get(source_index.len()) == Some(&b'/')
        {
            return Err(Error::InvalidRepository(
                "cannot move a directory into itself".into(),
            ));
        }
        if !directory && selected.len() != 1 {
            return Err(Error::InvalidRepository(
                "file source also has tracked descendants".into(),
            ));
        }

        let destination_path = work_tree.join(&destination);
        let destination_metadata = match self.filesystem().metadata(&destination_path) {
            Ok(metadata) => Some(metadata),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if directory && destination_metadata.is_some() {
            return Err(Error::AlreadyExists(destination));
        }
        if !directory {
            if destination_metadata.is_some_and(crate::Metadata::is_dir) {
                return Err(Error::IsDirectory(destination));
            }
            if destination_metadata.is_some() && !options.force {
                return Err(Error::AlreadyExists(destination));
            }
        }
        if source_metadata.is_some() {
            let parent = destination_path
                .parent()
                .ok_or_else(|| Error::InvalidPath(destination.clone()))?;
            if !self.filesystem().metadata(parent)?.is_dir() {
                return Err(Error::NotDirectory(parent.to_path_buf()));
            }
        }

        let selected_paths = selected
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<std::collections::BTreeSet<_>>();
        let mappings = selected
            .iter()
            .map(|entry| {
                let suffix = entry
                    .path()
                    .strip_prefix(source_index.as_slice())
                    .ok_or_else(|| {
                        Error::InvalidRepository("selected path escaped source prefix".into())
                    })?;
                let mut path = destination_index.clone();
                path.extend_from_slice(suffix);
                Ok((entry.path().to_vec(), path))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut replace_destination = false;
        for entry in index
            .entries()
            .iter()
            .filter(|entry| !selected_paths.contains(entry.path()))
        {
            for (_, new_path) in &mappings {
                if entry.path() == new_path && !directory && options.force {
                    if entry.stage() != 0 {
                        return Err(Error::InvalidRepository(
                            "cannot overwrite an unresolved destination".into(),
                        ));
                    }
                    replace_destination = true;
                    continue;
                }
                if paths_collide(entry.path(), new_path) {
                    return Err(Error::AlreadyExists(worktree_path(new_path)?));
                }
            }
        }
        if options.dry_run {
            return Ok(mappings.len());
        }

        if source_metadata.is_some() {
            if directory {
                move_worktree_tree(self, &source_path, &destination_path)?;
            } else {
                self.filesystem().rename(&source_path, &destination_path)?;
            }
            self.prune_empty_parents(work_tree, source.parent())?;
        }
        let mapping = mappings.into_iter().collect::<BTreeMap<_, _>>();
        let mut entries = Vec::with_capacity(index.entries().len());
        for entry in index.entries() {
            if let Some(path) = mapping.get(entry.path()) {
                entries.push(entry.clone().with_path(path.clone())?);
            } else if !(replace_destination && entry.path() == destination_index) {
                entries.push(entry.clone());
            }
        }
        let moved = mapping.len();
        self.write_index(&Index::new(index.version(), entries)?)?;
        Ok(moved)
    }

    /// Write the stage-zero index as a hierarchy of tree objects.
    ///
    /// # Errors
    /// Returns an error for unresolved merge stages, intent-to-add entries,
    /// file/directory collisions, unsupported modes, or object storage failure.
    pub fn write_index_tree(&self, index: &Index) -> Result<ObjectId> {
        let mut root = BTreeMap::new();
        for entry in index.entries() {
            if entry.stage() != 0 {
                return Err(Error::InvalidTree(format!(
                    "unmerged index entry `{}`",
                    String::from_utf8_lossy(entry.path())
                )));
            }
            if entry.intent_to_add() {
                return Err(Error::InvalidTree(format!(
                    "intent-to-add entry `{}` has no final content",
                    String::from_utf8_lossy(entry.path())
                )));
            }
            let components = entry.path().split(|byte| *byte == b'/').collect::<Vec<_>>();
            insert_node(&mut root, &components, entry.mode(), entry.id())?;
        }
        self.write_node_tree(&root)
    }

    /// Check out a tree into the worktree and replace the index.
    ///
    /// By default, modified tracked files and colliding untracked paths are
    /// preserved and reported as conflicts. Entries unchanged between the old
    /// index and target keep their local modifications, matching branch-switch
    /// behavior. `force` requests an exact materialization.
    ///
    /// # Errors
    /// Returns [`Error::CheckoutConflict`] before mutation when local data would
    /// be overwritten, or an object, filesystem, or index transaction error.
    #[allow(clippy::too_many_lines)]
    pub fn checkout_tree(&self, tree: ObjectId, options: &CheckoutOptions) -> Result<usize> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("cannot check out into a bare repository".into())
        })?;
        let mut desired = self.flattened_tree(tree, options.max_object_size)?;
        desired.sort_unstable_by(|left, right| left.path.cmp(&right.path));

        let current = self.read_index()?;
        let current_by_path = current
            .entries()
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| (entry.path().to_vec(), entry))
            .collect::<BTreeMap<_, _>>();
        let desired_by_path = desired
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>();

        if !options.force {
            let mut conflicts = Vec::new();
            for (path, current_entry) in &current_by_path {
                let unchanged_target = desired_by_path.get(path).is_some_and(|target| {
                    target.id == current_entry.id() && target.raw_mode == current_entry.mode()
                });
                if unchanged_target {
                    continue;
                }
                let full_path = work_tree.join(worktree_path(path)?);
                match self.filesystem().metadata(&full_path) {
                    Ok(_) if !self.worktree_matches(current_entry, &full_path)? => {
                        conflicts.push(String::from_utf8_lossy(path).into_owned());
                    }
                    Ok(_) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            for (path, target) in &desired_by_path {
                for slash in path
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| (*byte == b'/').then_some(index))
                {
                    let parent_path = &path[..slash];
                    let parent = work_tree.join(worktree_path(parent_path)?);
                    match self.filesystem().metadata(&parent) {
                        Ok(metadata) if !metadata.is_dir() => {
                            let tracked_parent_will_be_removed = current_by_path
                                .contains_key(parent_path)
                                && !desired_by_path.contains_key(parent_path);
                            if !tracked_parent_will_be_removed {
                                conflicts.push(String::from_utf8_lossy(parent_path).into_owned());
                            }
                        }
                        Ok(_) | Err(Error::NotFound(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
                if current_by_path.contains_key(path) || target.mode == EntryMode::Gitlink {
                    continue;
                }
                let full_path = work_tree.join(worktree_path(path)?);
                if self.filesystem().exists(&full_path)? {
                    conflicts.push(String::from_utf8_lossy(path).into_owned());
                }
            }
            conflicts.sort_unstable();
            conflicts.dedup();
            if !conflicts.is_empty() {
                return Err(Error::CheckoutConflict(conflicts));
            }
        }

        let mut removals = current_by_path
            .keys()
            .filter(|path| !desired_by_path.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        removals.sort_unstable_by_key(|path| std::cmp::Reverse(path.len()));
        let removals_set = removals.iter().cloned().collect::<BTreeSet<_>>();
        for path in &removals {
            let relative = worktree_path(path)?;
            if has_symlink_leading_path(self.filesystem(), work_tree, &relative)? {
                return Err(Error::BeyondSymbolicLink(relative));
            }
        }
        for target in &desired {
            let relative = worktree_path(&target.path)?;
            if has_symlink_leading_path_replaced(
                self.filesystem(),
                work_tree,
                &relative,
                &removals_set,
            )? {
                return Err(Error::BeyondSymbolicLink(relative));
            }
        }
        for path in removals {
            let relative = worktree_path(&path)?;
            let full_path = work_tree.join(&relative);
            match self.filesystem().metadata(&full_path) {
                Ok(metadata) if metadata.is_file() || metadata.is_symlink() => {
                    self.filesystem().remove_file(&full_path)?;
                    self.prune_empty_parents(work_tree, relative.parent())?;
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }

        let mut new_entries = Vec::with_capacity(desired.len());
        let mut written = 0;
        for target in desired {
            if !options.force
                && current_by_path
                    .get(&target.path)
                    .is_some_and(|current_entry| {
                        current_entry.id() == target.id && current_entry.mode() == target.raw_mode
                    })
            {
                new_entries.push((*current_by_path[&target.path]).clone());
                continue;
            }
            let relative = worktree_path(&target.path)?;
            let full_path = work_tree.join(&relative);
            if target.mode == EntryMode::Gitlink {
                self.filesystem().create_dir_all(&full_path)?;
            } else {
                if let Some(parent) = full_path.parent() {
                    self.filesystem().create_dir_all(parent)?;
                }
                if let Ok(metadata) = self.filesystem().metadata(&full_path) {
                    if metadata.is_dir() {
                        self.filesystem().remove_dir(&full_path)?;
                    } else {
                        self.filesystem().remove_file(&full_path)?;
                    }
                }
                let object = self.read_object(target.id, options.max_object_size)?;
                if object.kind() != target.mode.object_kind() {
                    return Err(Error::InvalidTree(format!(
                        "entry `{}` points to the wrong object type",
                        String::from_utf8_lossy(&target.path)
                    )));
                }
                if target.mode == EntryMode::Link {
                    self.filesystem()
                        .create_symlink(&full_path, object.data())?;
                } else {
                    self.filesystem().write(&full_path, object.data())?;
                    self.filesystem()
                        .set_executable(&full_path, target.mode == EntryMode::BlobExecutable)?;
                }
            }
            let metadata = self.filesystem().metadata(&full_path)?;
            new_entries.push(IndexEntry::new(
                target.path,
                target.raw_mode,
                target.id,
                index_stat(metadata.stat(), metadata.len()),
            )?);
            written += 1;
        }
        self.write_index(&Index::new(current.version(), new_entries)?)?;
        Ok(written)
    }

    pub(crate) fn flattened_tree(&self, id: ObjectId, max_size: usize) -> Result<Vec<TreeLeaf>> {
        let mut entries = Vec::new();
        self.flatten_tree(id, &[], max_size, &mut entries)?;
        Ok(entries)
    }

    fn flatten_tree(
        &self,
        id: ObjectId,
        prefix: &[u8],
        max_size: usize,
        output: &mut Vec<TreeLeaf>,
    ) -> Result<()> {
        for entry in self.read_tree(id, max_size)?.entries() {
            let mut path = prefix.to_owned();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.name());
            if entry.mode() == EntryMode::Tree {
                self.flatten_tree(entry.id(), &path, max_size, output)?;
            } else {
                output.push(TreeLeaf {
                    raw_mode: tree_mode(entry.mode()),
                    mode: entry.mode(),
                    path,
                    id: entry.id(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn worktree_matches(&self, entry: &IndexEntry, full_path: &Path) -> Result<bool> {
        let metadata = self.filesystem().metadata(full_path)?;
        let (contents, mode) = if metadata.is_symlink() {
            (self.filesystem().read_link(full_path)?, 0o120_000)
        } else if metadata.is_file() {
            (
                self.filesystem().read(full_path)?,
                if metadata.is_executable() {
                    0o100_755
                } else {
                    0o100_644
                },
            )
        } else if metadata.is_dir() && entry.mode() == 0o160_000 {
            return Ok(true);
        } else {
            return Ok(false);
        };
        Ok(mode == entry.mode() && ObjectId::compute(ObjectKind::Blob, &contents) == entry.id())
    }

    pub(crate) fn prune_empty_parents(
        &self,
        work_tree: &Path,
        mut parent: Option<&Path>,
    ) -> Result<()> {
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            let path = work_tree.join(relative);
            match self.filesystem().remove_dir(&path) {
                Ok(()) | Err(Error::NotFound(_) | Error::DirectoryNotEmpty(_)) => {}
                Err(error) => return Err(error),
            }
            parent = relative.parent();
        }
        Ok(())
    }

    fn write_node_tree(&self, nodes: &BTreeMap<Vec<u8>, IndexNode>) -> Result<ObjectId> {
        let mut entries = Vec::with_capacity(nodes.len());
        for (name, node) in nodes {
            let (mode, id) = match node {
                IndexNode::Leaf { mode, id } => (*mode, *id),
                IndexNode::Directory(children) => {
                    (EntryMode::Tree, self.write_node_tree(children)?)
                }
            };
            entries.push(TreeEntry::new(mode, name.clone(), id)?);
        }
        self.write_tree(&Tree::new(entries)?)
    }

    fn collect_entries(
        &self,
        work_tree: &Path,
        relative: &Path,
        tracked: &std::collections::BTreeSet<Vec<u8>>,
        ignores: &mut IgnoreMatcher,
        force: bool,
        output: &mut Vec<IndexEntry>,
    ) -> Result<()> {
        let path = work_tree.join(relative);
        let metadata = self.filesystem().metadata(&path)?;
        let index_path = index_path(relative)?;
        if metadata.is_dir() {
            ignores.add_worktree_patterns(self, work_tree, &index_path)?;
            if !force
                && !index_path.is_empty()
                && ignores.is_ignored(&index_path, true)
                && !tracked
                    .iter()
                    .any(|tracked| path_is_selected(tracked, &index_path))
            {
                return Ok(());
            }
            for child in self.filesystem().read_dir(&path)? {
                let child_relative = relative.join(child);
                if child_relative == Path::new(".git")
                    || work_tree.join(&child_relative) == self.git_dir()
                {
                    continue;
                }
                self.collect_entries(work_tree, &child_relative, tracked, ignores, force, output)?;
            }
            return Ok(());
        }

        if !force && !tracked.contains(&index_path) && ignores.is_ignored(&index_path, false) {
            return Ok(());
        }

        let (kind, contents, mode) = if metadata.is_symlink() {
            (
                ObjectKind::Blob,
                self.filesystem().read_link(&path)?,
                0o120_000,
            )
        } else if metadata.is_file() {
            (
                ObjectKind::Blob,
                self.filesystem().read(&path)?,
                if metadata.is_executable() {
                    0o100_755
                } else {
                    0o100_644
                },
            )
        } else {
            return Err(Error::InvalidPath(path));
        };
        let id = self.write_object(kind, &contents)?;
        output.push(IndexEntry::new(
            index_path,
            mode,
            id,
            index_stat(metadata.stat(), metadata.len()),
        )?);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_add_plan(
        &self,
        work_tree: &Path,
        relative: &Path,
        tracked: &std::collections::BTreeSet<Vec<u8>>,
        ignores: &mut IgnoreMatcher,
        options: &AddTransactionOptions,
        explicit: bool,
        output: &mut BTreeMap<Vec<u8>, PendingAdd>,
        skipped: &mut std::collections::BTreeSet<Vec<u8>>,
    ) -> Result<()> {
        let path = work_tree.join(relative);
        let metadata = match self.filesystem().metadata(&path) {
            Ok(metadata) => metadata,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let index_path = index_path(relative)?;
        if metadata.is_dir() {
            ignores.add_worktree_patterns(self, work_tree, &index_path)?;
            if !options.force
                && !index_path.is_empty()
                && ignores.is_ignored(&index_path, true)
                && !tracked
                    .iter()
                    .any(|candidate| path_is_selected(candidate, &index_path))
            {
                if explicit {
                    return Err(Error::IgnoredPath(relative.to_path_buf()));
                }
                skipped.insert(index_path);
                return Ok(());
            }
            for child in self.filesystem().read_dir(&path)? {
                let child_relative = relative.join(child);
                if child_relative == Path::new(".git")
                    || work_tree.join(&child_relative) == self.git_dir()
                {
                    continue;
                }
                self.collect_add_plan(
                    work_tree,
                    &child_relative,
                    tracked,
                    ignores,
                    options,
                    false,
                    output,
                    skipped,
                )?;
            }
            return Ok(());
        }
        let is_tracked = tracked.contains(&index_path);
        if options.update_only && !is_tracked {
            return Ok(());
        }
        if !options.force && !is_tracked && ignores.is_ignored(&index_path, false) {
            if explicit {
                return Err(Error::IgnoredPath(relative.to_path_buf()));
            }
            skipped.insert(index_path);
            return Ok(());
        }
        if metadata.len() > options.max_file_size as u64 {
            return Err(Error::ObjectTooLarge {
                declared: metadata.len(),
                limit: options.max_file_size,
            });
        }
        let (contents, mode) = if metadata.is_symlink() {
            (self.filesystem().read_link(&path)?, 0o120_000)
        } else if metadata.is_file() {
            let executable = options.executable.unwrap_or(metadata.is_executable());
            (
                self.filesystem().read(&path)?,
                if executable { 0o100_755 } else { 0o100_644 },
            )
        } else {
            return Err(Error::InvalidPath(path));
        };
        if contents.len() > options.max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: contents.len() as u64,
                limit: options.max_file_size,
            });
        }
        output.insert(
            index_path,
            PendingAdd {
                contents,
                mode,
                stat: index_stat(metadata.stat(), metadata.len()),
            },
        );
        Ok(())
    }
}

struct PendingAdd {
    contents: Vec<u8>,
    mode: u32,
    stat: StatData,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeLeaf {
    pub(crate) raw_mode: u32,
    pub(crate) mode: EntryMode,
    pub(crate) path: Vec<u8>,
    pub(crate) id: ObjectId,
}

const fn tree_mode(mode: EntryMode) -> u32 {
    match mode {
        EntryMode::Blob => 0o100_644,
        EntryMode::BlobExecutable => 0o100_755,
        EntryMode::Link => 0o120_000,
        EntryMode::Tree => 0o040_000,
        EntryMode::Gitlink => 0o160_000,
    }
}

#[derive(Debug)]
enum IndexNode {
    Leaf { mode: EntryMode, id: ObjectId },
    Directory(BTreeMap<Vec<u8>, IndexNode>),
}

fn insert_node(
    nodes: &mut BTreeMap<Vec<u8>, IndexNode>,
    components: &[&[u8]],
    raw_mode: u32,
    id: ObjectId,
) -> Result<()> {
    let (name, remainder) = components
        .split_first()
        .ok_or_else(|| Error::InvalidTree("empty index path".into()))?;
    if remainder.is_empty() {
        if nodes.contains_key(*name) {
            return Err(Error::InvalidTree("index file/directory collision".into()));
        }
        nodes.insert(
            name.to_vec(),
            IndexNode::Leaf {
                mode: entry_mode(raw_mode)?,
                id,
            },
        );
        return Ok(());
    }
    let node = nodes
        .entry(name.to_vec())
        .or_insert_with(|| IndexNode::Directory(BTreeMap::new()));
    match node {
        IndexNode::Directory(children) => insert_node(children, remainder, raw_mode, id),
        IndexNode::Leaf { .. } => Err(Error::InvalidTree("index file/directory collision".into())),
    }
}

fn entry_mode(mode: u32) -> Result<EntryMode> {
    match mode {
        0o100_644 => Ok(EntryMode::Blob),
        0o100_755 => Ok(EntryMode::BlobExecutable),
        0o120_000 => Ok(EntryMode::Link),
        0o160_000 => Ok(EntryMode::Gitlink),
        0o040_000 => Ok(EntryMode::Tree),
        _ => Err(Error::InvalidTree(format!(
            "unsupported index mode {mode:o}"
        ))),
    }
}

/// Return whether any leading directory component of a repository-relative
/// path is a symbolic link on disk.
///
/// Mirrors git's `has_symlink_leading_path` refusal ("pathspec is beyond a
/// symbolic link"): every directory component except the final one is examined
/// without following symlinks. The final component is intentionally exempt so
/// that operating on a symlink itself (adding, moving, or removing one) stays
/// permitted, matching git.
///
/// Detection relies on [`FileSystem::metadata`] reporting symlink status for
/// each leading component. On Windows, directory junctions and other reparse
/// points are only blocked when the host reports them as symlinks; add
/// reparse-point detection to `HostFileSystem::metadata` if junction traversal
/// must be rejected there.
///
/// # Errors
/// Returns storage errors other than an absent leading component, which is
/// treated as not being a symlink.
pub(crate) fn has_symlink_leading_path(
    filesystem: &dyn FileSystem,
    work_tree: &Path,
    relative: &Path,
) -> Result<bool> {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        if let Component::Normal(part) = component {
            prefix.push(part);
            if prefix == relative {
                continue;
            }
            let path = work_tree.join(&prefix);
            match filesystem.metadata(&path) {
                Ok(metadata) if metadata.is_symlink() => return Ok(true),
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(false)
}

/// Like [`has_symlink_leading_path`], but leading components that this checkout
/// replaces with real directories (currently-tracked paths present in
/// `removals`) are not treated as symlink traversal.
///
/// Git handles the symlink- or file-to-directory transition by removing the
/// old entry before materializing the directory, so those leading components
/// are never traversed; only symlinks that survive the checkout block the path.
pub(crate) fn has_symlink_leading_path_replaced(
    filesystem: &dyn FileSystem,
    work_tree: &Path,
    relative: &Path,
    removals: &BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        if let Component::Normal(part) = component {
            prefix.push(part);
            if prefix == relative {
                continue;
            }
            if removals.contains(&index_path(&prefix)?) {
                continue;
            }
            let path = work_tree.join(&prefix);
            match filesystem.metadata(&path) {
                Ok(metadata) if metadata.is_symlink() => return Ok(true),
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(false)
}

fn normalize_relative(path: &Path) -> Result<PathBuf> {
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
    Ok(normalized)
}

pub(crate) fn index_stat(stat: FileStat, len: u64) -> StatData {
    StatData {
        ctime_seconds: stat.ctime_seconds,
        ctime_nanoseconds: stat.ctime_nanoseconds,
        mtime_seconds: stat.mtime_seconds,
        mtime_nanoseconds: stat.mtime_nanoseconds,
        device: stat.device,
        inode: stat.inode,
        uid: stat.uid,
        gid: stat.gid,
        size: u32::try_from(len & u64::from(u32::MAX)).expect("value was masked to u32"),
    }
}

fn path_is_selected(candidate: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || candidate == prefix
        || (candidate.starts_with(prefix) && candidate.get(prefix.len()) == Some(&b'/'))
}

fn paths_collide(left: &[u8], right: &[u8]) -> bool {
    path_is_selected(left, right) || path_is_selected(right, left)
}

fn move_worktree_tree(repository: &Repository, source: &Path, destination: &Path) -> Result<()> {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    collect_worktree_move(
        repository,
        source,
        destination,
        &mut directories,
        &mut files,
    )?;
    let mut created: Vec<PathBuf> = Vec::new();
    for (_, directory) in &directories {
        if let Err(error) = repository.filesystem().create_dir_all(directory) {
            for path in created.iter().rev() {
                let _ = repository.filesystem().remove_dir(path);
            }
            return Err(error);
        }
        created.push(directory.clone());
    }
    let mut moved = Vec::new();
    for (from, to) in &files {
        if let Err(error) = repository.filesystem().rename(from, to) {
            rollback_worktree_move(repository, &directories, &moved, &created);
            return Err(error);
        }
        moved.push((from.clone(), to.clone()));
    }
    for (directory, _) in directories.iter().rev() {
        if let Err(error) = repository.filesystem().remove_dir(directory) {
            rollback_worktree_move(repository, &directories, &moved, &created);
            return Err(error);
        }
    }
    Ok(())
}

fn collect_worktree_move(
    repository: &Repository,
    source: &Path,
    destination: &Path,
    directories: &mut Vec<(PathBuf, PathBuf)>,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    directories.push((source.to_path_buf(), destination.to_path_buf()));
    for child in repository.filesystem().read_dir(source)? {
        let from = source.join(&child);
        let to = destination.join(child);
        if repository.filesystem().metadata(&from)?.is_dir() {
            collect_worktree_move(repository, &from, &to, directories, files)?;
        } else {
            files.push((from, to));
        }
    }
    Ok(())
}

fn rollback_worktree_move(
    repository: &Repository,
    directories: &[(PathBuf, PathBuf)],
    moved: &[(PathBuf, PathBuf)],
    created: &[PathBuf],
) {
    for (source, _) in directories {
        let _ = repository.filesystem().create_dir_all(source);
    }
    for (from, to) in moved.iter().rev() {
        let _ = repository.filesystem().rename(to, from);
    }
    for path in created.iter().rev() {
        let _ = repository.filesystem().remove_dir(path);
    }
}

pub(crate) fn remove_worktree_tree(
    repository: &Repository,
    path: &Path,
    force: bool,
) -> Result<()> {
    let children = repository.filesystem().read_dir(path)?;
    if !force && !children.is_empty() {
        return Err(Error::DirectoryNotEmpty(path.to_path_buf()));
    }
    for child in children {
        let child_path = path.join(child);
        let metadata = repository.filesystem().metadata(&child_path)?;
        if metadata.is_dir() {
            remove_worktree_tree(repository, &child_path, true)?;
        } else {
            repository.filesystem().remove_file(&child_path)?;
        }
    }
    repository.filesystem().remove_dir(path)
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn worktree_path(path: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut output = PathBuf::new();
    for component in path.split(|byte| *byte == b'/') {
        output.push(OsStr::from_bytes(component));
    }
    Ok(output)
}

#[cfg(not(unix))]
pub(crate) fn worktree_path(path: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(path)
        .map_err(|_| Error::InvalidPath(PathBuf::from("non-UTF-8 index path")))?;
    Ok(text.split('/').collect())
}

#[cfg(not(unix))]
fn index_path(path: &Path) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for component in path.components() {
        if let Component::Normal(value) = component {
            let value = value
                .to_str()
                .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))?;
            if !output.is_empty() {
                output.push(b'/');
            }
            output.extend_from_slice(value.as_bytes());
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        FileSystem, HostFileSystem, IndexVersion, InitOptions, MemoryFileSystem,
    };

    #[test]
    fn recursively_adds_files_executables_and_symlinks_in_memory() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/src")).unwrap();
        fs.write(Path::new("repo/README"), b"readme\n").unwrap();
        fs.write(Path::new("repo/src/run"), b"#!/bin/sh\n").unwrap();
        fs.set_executable(Path::new("repo/src/run"), true).unwrap();
        fs.create_symlink(Path::new("repo/link"), b"src/run")
            .unwrap();

        assert_eq!(repository.add(".").unwrap(), 3);
        let index = repository.read_index().unwrap();
        assert_eq!(
            index
                .entries()
                .iter()
                .map(IndexEntry::path)
                .collect::<Vec<_>>(),
            [
                b"README".as_slice(),
                b"link".as_slice(),
                b"src/run".as_slice()
            ]
        );
        assert_eq!(index.entries()[1].mode(), 0o120_000);
        assert_eq!(index.entries()[2].mode(), 0o100_755);
        assert_eq!(
            repository
                .read_object(index.entries()[1].id(), 1024)
                .unwrap()
                .data(),
            b"src/run"
        );
    }

    #[test]
    fn adding_directory_stages_deletions_below_it() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/dir/a"), b"a").unwrap();
        fs.write(Path::new("repo/dir/b"), b"b").unwrap();
        repository.add("dir").unwrap();
        fs.remove_file(Path::new("repo/dir/b")).unwrap();
        assert_eq!(repository.add("dir").unwrap(), 1);
        assert_eq!(
            repository.read_index().unwrap().entries()[0].path(),
            b"dir/a"
        );
    }

    #[test]
    fn multi_add_updates_adds_and_removes_in_one_index_write() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/dir/keep"), b"old").unwrap();
        fs.write(Path::new("repo/dir/remove"), b"gone").unwrap();
        repository.add("dir").unwrap();
        fs.write(Path::new("repo/dir/keep"), b"new").unwrap();
        fs.remove_file(Path::new("repo/dir/remove")).unwrap();
        fs.write(Path::new("repo/dir/add"), b"added").unwrap();

        let report = repository
            .add_paths(&["dir"], &AddTransactionOptions::default())
            .unwrap();
        assert_eq!(report.staged, [b"dir/add".to_vec(), b"dir/keep".to_vec()]);
        assert_eq!(report.removed, [b"dir/remove".to_vec()]);
        let index = repository.read_index().unwrap();
        assert_eq!(index.entries().len(), 2);
        assert_eq!(
            repository
                .read_object(index.entries()[1].id(), 32)
                .unwrap()
                .data(),
            b"new"
        );
    }

    #[test]
    fn add_transaction_supports_dry_run_update_intent_chmod_and_limits() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/tracked"), b"old").unwrap();
        repository.add("tracked").unwrap();
        fs.write(Path::new("repo/tracked"), b"new").unwrap();
        fs.write(Path::new("repo/untracked"), b"value").unwrap();
        let before = repository.read_index().unwrap().encode().unwrap();
        let dry = repository
            .add_paths(
                &["tracked", "untracked"],
                &AddTransactionOptions {
                    dry_run: true,
                    executable: Some(true),
                    ..AddTransactionOptions::default()
                },
            )
            .unwrap();
        assert_eq!(dry.staged.len(), 2);
        assert_eq!(repository.read_index().unwrap().encode().unwrap(), before);

        repository
            .add_paths(
                &["tracked", "untracked"],
                &AddTransactionOptions {
                    update_only: true,
                    executable: Some(true),
                    ..AddTransactionOptions::default()
                },
            )
            .unwrap();
        let index = repository.read_index().unwrap();
        assert_eq!(index.entries().len(), 1);
        assert_eq!(index.entries()[0].mode(), 0o100_755);

        repository
            .add_paths(
                &["untracked"],
                &AddTransactionOptions {
                    intent_to_add: true,
                    ..AddTransactionOptions::default()
                },
            )
            .unwrap();
        let intent = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.path() == b"untracked")
            .unwrap()
            .clone();
        assert!(intent.intent_to_add());
        assert_eq!(intent.id(), ObjectId::compute(ObjectKind::Blob, b""));
        assert!(matches!(
            repository.add_paths(
                &["untracked"],
                &AddTransactionOptions {
                    max_file_size: 4,
                    ..AddTransactionOptions::default()
                }
            ),
            Err(Error::ObjectTooLarge { .. })
        ));
    }

    #[test]
    fn never_adds_repository_metadata() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        assert_eq!(repository.add(".").unwrap(), 0);
        assert!(repository.read_index().unwrap().entries().is_empty());
    }

    #[test]
    fn recursive_add_skips_ignored_untracked_but_updates_tracked_files() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/tracked.log"), b"old")
            .unwrap();
        repository.add("tracked.log").unwrap();
        filesystem
            .write(Path::new("repo/.gitignore"), b"*.log\nbuild/\n")
            .unwrap();
        filesystem
            .write(Path::new("repo/tracked.log"), b"new")
            .unwrap();
        filesystem
            .write(Path::new("repo/untracked.log"), b"skip")
            .unwrap();
        filesystem.create_dir_all(Path::new("repo/build")).unwrap();
        filesystem
            .write(Path::new("repo/build/output"), b"skip")
            .unwrap();
        filesystem
            .write(Path::new("repo/visible.txt"), b"add")
            .unwrap();

        assert!(matches!(
            repository.add("untracked.log"),
            Err(Error::IgnoredPath(_))
        ));
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .all(|entry| { entry.path() != b"untracked.log" })
        );

        repository
            .add_with_options("untracked.log", &AddOptions { force: true })
            .unwrap();
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| { entry.path() == b"untracked.log" })
        );
        let force_index = repository.read_index().unwrap();
        let retained = force_index
            .entries()
            .iter()
            .filter(|entry| entry.path() != b"untracked.log")
            .cloned()
            .collect();
        repository
            .write_index(&Index::new(force_index.version(), retained).unwrap())
            .unwrap();

        repository.add(".").unwrap();
        let index = repository.read_index().unwrap();
        let paths = index
            .entries()
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                b".gitignore".to_vec(),
                b"tracked.log".to_vec(),
                b"visible.txt".to_vec()
            ]
        );
        let tracked = index
            .entries()
            .iter()
            .find(|entry| entry.path() == b"tracked.log")
            .unwrap();
        assert_eq!(
            repository.read_object(tracked.id(), 1024).unwrap().data(),
            b"new"
        );
    }

    #[test]
    fn writes_nested_index_as_tree_hierarchy() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/top"), b"top").unwrap();
        fs.write(Path::new("repo/dir/nested"), b"nested").unwrap();
        repository.add(".").unwrap();
        let root_id = repository
            .write_index_tree(&repository.read_index().unwrap())
            .unwrap();
        let root = repository.read_tree(root_id, 4096).unwrap();
        assert_eq!(
            root.entries()
                .iter()
                .map(TreeEntry::name)
                .collect::<Vec<_>>(),
            [b"dir".as_slice(), b"top".as_slice()]
        );
        let directory = root
            .entries()
            .iter()
            .find(|entry| entry.name() == b"dir")
            .unwrap();
        assert_eq!(directory.mode(), EntryMode::Tree);
        assert_eq!(
            repository
                .read_tree(directory.id(), 4096)
                .unwrap()
                .entries()[0]
                .name(),
            b"nested"
        );
    }

    #[test]
    fn checkout_materializes_modes_and_refuses_modified_overwrites() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let old = repository.write_object(ObjectKind::Blob, b"old").unwrap();
        let new = repository.write_object(ObjectKind::Blob, b"new").unwrap();
        let script = repository
            .write_object(ObjectKind::Blob, b"#!/bin/sh\n")
            .unwrap();
        let link = repository
            .write_object(ObjectKind::Blob, b"script")
            .unwrap();
        let first = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), old).unwrap(),
                    TreeEntry::new(EntryMode::BlobExecutable, b"script".to_vec(), script).unwrap(),
                    TreeEntry::new(EntryMode::Link, b"link".to_vec(), link).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            repository
                .checkout_tree(first, &CheckoutOptions::default())
                .unwrap(),
            3
        );
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"old");
        assert!(
            fs.metadata(Path::new("repo/script"))
                .unwrap()
                .is_executable()
        );
        assert_eq!(fs.read_link(Path::new("repo/link")).unwrap(), b"script");

        let second = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), new).unwrap(),
                    TreeEntry::new(EntryMode::BlobExecutable, b"script".to_vec(), script).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        fs.write(Path::new("repo/file"), b"local change").unwrap();
        assert!(matches!(
            repository.checkout_tree(second, &CheckoutOptions::default()),
            Err(Error::CheckoutConflict(_))
        ));
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"local change");
        assert!(fs.exists(Path::new("repo/link")).unwrap());

        repository
            .checkout_tree(
                second,
                &CheckoutOptions {
                    force: true,
                    ..CheckoutOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"new");
        assert!(!fs.exists(Path::new("repo/link")).unwrap());
    }

    #[test]
    fn checkout_preserves_modifications_when_target_entry_is_unchanged() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"tracked")
            .unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        repository
            .checkout_tree(tree, &CheckoutOptions::default())
            .unwrap();
        fs.write(Path::new("repo/file"), b"modified").unwrap();
        assert_eq!(
            repository
                .checkout_tree(tree, &CheckoutOptions::default())
                .unwrap(),
            0
        );
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"modified");
    }

    #[test]
    fn checkout_refuses_an_untracked_collision_before_mutating() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"target")
            .unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"untracked".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        fs.write(Path::new("repo/untracked"), b"local").unwrap();
        assert!(matches!(
            repository.checkout_tree(tree, &CheckoutOptions::default()),
            Err(Error::CheckoutConflict(_))
        ));
        assert_eq!(fs.read(Path::new("repo/untracked")).unwrap(), b"local");
        assert!(repository.read_index().unwrap().entries().is_empty());
    }

    fn removal_fixture() -> (Repository, MemoryFileSystem) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/dir/one"), b"one").unwrap();
        fs.write(Path::new("repo/dir/two"), b"two").unwrap();
        fs.write(Path::new("repo/root"), b"root").unwrap();
        repository.add("dir").unwrap();
        repository.add("root").unwrap();
        let signature = crate::Signature::new("Remove", "remove@example.com", 100, 0).unwrap();
        repository
            .commit_index(
                b"base",
                &signature,
                &signature,
                &crate::CommitOptions::default(),
            )
            .unwrap();
        (repository, fs)
    }

    #[test]
    fn removes_clean_files_and_requires_recursive_for_prefixes() {
        let (repository, fs) = removal_fixture();
        assert!(
            repository
                .remove(&["dir"], &RemoveOptions::default())
                .is_err()
        );
        assert!(fs.exists(Path::new("repo/dir/one")).unwrap());
        let removed = repository
            .remove(
                &["dir"],
                &RemoveOptions {
                    recursive: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(removed, vec![b"dir/one".to_vec(), b"dir/two".to_vec()]);
        assert!(!fs.exists(Path::new("repo/dir")).unwrap());
        assert_eq!(repository.read_index().unwrap().entries().len(), 1);
    }

    #[test]
    fn protects_local_and_staged_changes_unless_forced() {
        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"local").unwrap();
        assert!(matches!(
            repository.remove(&["root"], &RemoveOptions::default()),
            Err(Error::CheckoutConflict(_))
        ));
        assert!(fs.exists(Path::new("repo/root")).unwrap());
        repository
            .remove(
                &["root"],
                &RemoveOptions {
                    force: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert!(!fs.exists(Path::new("repo/root")).unwrap());

        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"staged").unwrap();
        repository.add("root").unwrap();
        assert!(
            repository
                .remove(&["root"], &RemoveOptions::default())
                .is_err()
        );
        repository
            .remove(
                &["root"],
                &RemoveOptions {
                    cached: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/root")).unwrap(), b"staged");
    }

    #[test]
    fn cached_removal_requires_index_to_match_head_or_worktree() {
        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"staged").unwrap();
        repository.add("root").unwrap();
        fs.write(Path::new("repo/root"), b"different local")
            .unwrap();
        assert!(
            repository
                .remove(
                    &["root"],
                    &RemoveOptions {
                        cached: true,
                        ..RemoveOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.path() == b"root")
        );

        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"local only").unwrap();
        repository
            .remove(
                &["root"],
                &RemoveOptions {
                    cached: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/root")).unwrap(), b"local only");
    }

    #[test]
    fn accepts_missing_and_unmerged_paths_and_preflights_dry_run() {
        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"staged then missing")
            .unwrap();
        repository.add("root").unwrap();
        fs.remove_file(Path::new("repo/root")).unwrap();
        repository
            .remove(&["root"], &RemoveOptions::default())
            .unwrap();

        let blob = repository.write_object(ObjectKind::Blob, b"ours").unwrap();
        let mut entries = repository.read_index().unwrap().entries().to_vec();
        entries.push(
            IndexEntry::with_stage("conflict", 0o100_644, blob, StatData::default(), 2).unwrap(),
        );
        entries.push(
            IndexEntry::with_stage("conflict", 0o100_644, blob, StatData::default(), 3).unwrap(),
        );
        repository
            .write_index(&Index::new(IndexVersion::V3, entries).unwrap())
            .unwrap();
        repository
            .remove(&["conflict"], &RemoveOptions::default())
            .unwrap();
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .all(|entry| entry.path() != b"conflict")
        );

        let before = repository.read_index().unwrap();
        let selected = repository
            .remove(
                &["dir/one", "absent"],
                &RemoveOptions {
                    dry_run: true,
                    ignore_unmatched: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(selected, vec![b"dir/one".to_vec()]);
        assert_eq!(repository.read_index().unwrap(), before);
        assert!(fs.exists(Path::new("repo/dir/one")).unwrap());
    }

    #[test]
    fn sparse_entries_require_explicit_inclusion() {
        let (repository, fs) = removal_fixture();
        let entries = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .cloned()
            .map(|entry| {
                if entry.path() == b"root" {
                    entry.with_skip_worktree(true)
                } else {
                    entry
                }
            })
            .collect::<Vec<_>>();
        repository
            .write_index(&Index::new(IndexVersion::V3, entries).unwrap())
            .unwrap();
        assert!(
            repository
                .remove(
                    &["root"],
                    &RemoveOptions {
                        cached: true,
                        ..RemoveOptions::default()
                    }
                )
                .is_err()
        );
        repository
            .remove(
                &["root"],
                &RemoveOptions {
                    cached: true,
                    include_sparse: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert!(fs.exists(Path::new("repo/root")).unwrap());
    }

    #[test]
    fn moves_file_with_staged_and_local_layers_intact() {
        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/root"), b"staged").unwrap();
        repository.add("root").unwrap();
        let staged = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.path() == b"root")
            .unwrap()
            .id();
        fs.write(Path::new("repo/root"), b"local").unwrap();
        assert_eq!(
            repository
                .move_path("root", "renamed", &MoveOptions::default())
                .unwrap(),
            1
        );
        assert!(!fs.exists(Path::new("repo/root")).unwrap());
        assert_eq!(fs.read(Path::new("repo/renamed")).unwrap(), b"local");
        let entry = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.path() == b"renamed")
            .unwrap()
            .clone();
        assert_eq!(entry.id(), staged);
    }

    #[test]
    fn moves_directory_with_tracked_and_untracked_contents() {
        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/dir/untracked"), b"extra").unwrap();
        assert_eq!(
            repository
                .move_path("dir", "moved", &MoveOptions::default())
                .unwrap(),
            2
        );
        assert!(!fs.exists(Path::new("repo/dir")).unwrap());
        assert_eq!(fs.read(Path::new("repo/moved/one")).unwrap(), b"one");
        assert_eq!(
            fs.read(Path::new("repo/moved/untracked")).unwrap(),
            b"extra"
        );
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .map(|entry| entry.path().to_vec())
                .collect::<Vec<_>>(),
            vec![
                b"moved/one".to_vec(),
                b"moved/two".to_vec(),
                b"root".to_vec()
            ]
        );
        assert!(
            repository
                .move_path("moved", "moved/inside", &MoveOptions::default())
                .is_err()
        );
    }

    #[test]
    fn force_replaces_only_file_destinations_and_dry_run_is_immutable() {
        let (repository, fs) = removal_fixture();
        assert!(
            repository
                .move_path("root", "dir/one", &MoveOptions::default())
                .is_err()
        );
        assert_eq!(fs.read(Path::new("repo/root")).unwrap(), b"root");
        let before = repository.read_index().unwrap();
        assert_eq!(
            repository
                .move_path(
                    "root",
                    "dir/one",
                    &MoveOptions {
                        force: true,
                        dry_run: true,
                        ..MoveOptions::default()
                    }
                )
                .unwrap(),
            1
        );
        assert_eq!(repository.read_index().unwrap(), before);
        repository
            .move_path(
                "root",
                "dir/one",
                &MoveOptions {
                    force: true,
                    ..MoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/dir/one")).unwrap(), b"root");
        assert!(
            !repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.path() == b"root")
        );

        let (repository, fs) = removal_fixture();
        fs.write(Path::new("repo/untracked"), b"occupied").unwrap();
        assert!(
            repository
                .move_path("root", "untracked", &MoveOptions::default())
                .is_err()
        );
        repository
            .move_path(
                "root",
                "untracked",
                &MoveOptions {
                    force: true,
                    ..MoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/untracked")).unwrap(), b"root");
    }

    #[test]
    fn rejects_unresolved_and_handles_opted_in_sparse_index_only_move() {
        let (repository, fs) = removal_fixture();
        let blob = repository
            .write_object(ObjectKind::Blob, b"conflict")
            .unwrap();
        let mut entries = repository.read_index().unwrap().entries().to_vec();
        entries.push(
            IndexEntry::with_stage("conflict", 0o100_644, blob, StatData::default(), 2).unwrap(),
        );
        repository
            .write_index(&Index::new(IndexVersion::V3, entries).unwrap())
            .unwrap();
        assert!(
            repository
                .move_path("conflict", "resolved", &MoveOptions::default())
                .is_err()
        );

        let entries = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| entry.path() != b"conflict")
            .cloned()
            .map(|entry| {
                if entry.path() == b"root" {
                    entry.with_skip_worktree(true)
                } else {
                    entry
                }
            })
            .collect::<Vec<_>>();
        repository
            .write_index(&Index::new(IndexVersion::V3, entries).unwrap())
            .unwrap();
        fs.remove_file(Path::new("repo/root")).unwrap();
        assert!(
            repository
                .move_path("root", "renamed", &MoveOptions::default())
                .is_err()
        );
        repository
            .move_path(
                "root",
                "renamed",
                &MoveOptions {
                    include_sparse: true,
                    ..MoveOptions::default()
                },
            )
            .unwrap();
        let renamed = repository
            .read_index()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.path() == b"renamed")
            .unwrap()
            .clone();
        assert!(renamed.skip_worktree());
        assert!(!fs.exists(Path::new("repo/renamed")).unwrap());
    }

    /// Stage an index entry for `link/secret.txt` so symlink-guard checks that
    /// require a tracked source are reachable even though `add` now refuses it.
    fn stage_through_link(repository: &Repository) {
        let index = repository.read_index().unwrap();
        let secret = ObjectId::compute(ObjectKind::Blob, b"top-secret\n");
        let mut entries = index.entries().to_vec();
        entries.push(
            IndexEntry::new("link/secret.txt", 0o100_644, secret, StatData::default()).unwrap(),
        );
        repository
            .write_index(&Index::new(index.version(), entries).unwrap())
            .unwrap();
    }

    fn symlink_escape_fixture() -> (tempfile::TempDir, HostFileSystem, Repository) {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"top-secret\n").unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        (base, fs, repository)
    }

    #[test]
    fn rejects_adding_through_a_symlinked_directory() {
        let (base, _, repository) = symlink_escape_fixture();
        assert!(matches!(
            repository.add("link/secret.txt"),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(repository.read_index().unwrap().entries().is_empty());
        assert!(base.path().join("outside/secret.txt").exists());
    }

    #[test]
    fn rejects_moving_through_a_symlinked_directory_before_renaming() {
        let (base, _, repository) = symlink_escape_fixture();
        stage_through_link(&repository);
        assert!(matches!(
            repository.move_path("link/secret.txt", "moved.txt", &MoveOptions::default()),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(
            base.path().join("outside/secret.txt").exists(),
            "external file must not be relocated"
        );
        assert!(
            !base.path().join("repo/moved.txt").exists(),
            "no file may be moved into the repository"
        );
    }

    #[test]
    fn rejects_removing_through_a_symlinked_directory() {
        let (base, _, repository) = symlink_escape_fixture();
        stage_through_link(&repository);
        assert!(matches!(
            repository.remove(&["link/secret.txt"], &RemoveOptions::default()),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(base.path().join("outside/secret.txt").exists());
    }

    #[test]
    fn rejects_checkout_through_a_symlinked_directory() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"secret\n")
            .unwrap();
        let link_tree = repository
            .write_tree(
                &Tree::new(vec![TreeEntry::new(
                    EntryMode::Blob,
                    b"secret.txt".to_vec(),
                    blob,
                )
                .unwrap()])
                .unwrap(),
            )
            .unwrap();
        let tree = repository
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
            repository.checkout_tree(
                tree,
                &CheckoutOptions {
                    force: true,
                    ..CheckoutOptions::default()
                }
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(
            !outside.join("secret.txt").exists(),
            "checkout must not write through a symlinked directory"
        );
    }

    #[test]
    fn adding_and_moving_through_real_directories_still_works() {
        let base = tempfile::tempdir().unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/dir/file"), b"contents").unwrap();
        assert_eq!(repository.add("dir/file").unwrap(), 1);
        assert_eq!(
            repository
                .move_path("dir/file", "moved.txt", &MoveOptions::default())
                .unwrap(),
            1
        );
        assert_eq!(fs.read(Path::new("repo/moved.txt")).unwrap(), b"contents");
        assert!(!fs.exists(Path::new("repo/dir/file")).unwrap());
    }

    #[test]
    fn moving_a_symlink_file_itself_is_still_allowed() {
        let base = tempfile::tempdir().unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/target"), b"contents").unwrap();
        fs.create_symlink(Path::new("repo/link"), b"target").unwrap();
        repository.add("link").unwrap();
        assert_eq!(
            repository
                .move_path("link", "renamed-link", &MoveOptions::default())
                .unwrap(),
            1
        );
        assert_eq!(
            fs.read_link(Path::new("repo/renamed-link")).unwrap(),
            b"target"
        );
        assert!(!fs.exists(Path::new("repo/link")).unwrap());
    }

    #[test]
    fn checkout_replaces_a_tracked_symlink_pointing_outside_with_a_directory() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        repository.add("link").unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"nested\n")
            .unwrap();
        let link_tree = repository
            .write_tree(
                &Tree::new(vec![TreeEntry::new(
                    EntryMode::Blob,
                    b"sub.txt".to_vec(),
                    blob,
                )
                .unwrap()])
                .unwrap(),
            )
            .unwrap();
        let desired = repository
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
        assert_eq!(
            repository
                .checkout_tree(desired, &CheckoutOptions::default())
                .unwrap(),
            1
        );
        assert!(!fs.metadata(Path::new("repo/link")).unwrap().is_symlink());
        assert_eq!(fs.read(Path::new("repo/link/sub.txt")).unwrap(), b"nested\n");
        assert!(
            !outside.join("sub.txt").exists(),
            "checkout must not write through the replaced symlink"
        );
    }

    #[test]
    fn sparse_move_rejects_a_symlinked_destination() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/src"), b"contents").unwrap();
        repository.add("src").unwrap();
        fs.create_symlink(Path::new("repo/link"), b"outside").unwrap();
        let index = repository.read_index().unwrap();
        let entries = index
            .entries()
            .iter()
            .map(|entry| entry.clone().with_skip_worktree(true))
            .collect::<Vec<_>>();
        repository
            .write_index(&Index::new(IndexVersion::V3, entries).unwrap())
            .unwrap();
        assert!(matches!(
            repository.move_path(
                "src",
                "link/dst.txt",
                &MoveOptions {
                    include_sparse: true,
                    ..MoveOptions::default()
                }
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
    }

    #[test]
    fn move_rejects_a_deep_symlinked_destination_ancestor() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/src"), b"contents").unwrap();
        repository.add("src").unwrap();
        fs.create_symlink(Path::new("repo/link"), b"outside").unwrap();
        assert!(matches!(
            repository.move_path("src", "link/sub/dst.txt", &MoveOptions::default()),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(fs.exists(Path::new("repo/src")).unwrap());
        assert!(!fs.exists(Path::new("repo/link/sub/dst.txt")).unwrap());
    }

    #[test]
    fn cached_removal_ignores_symlinked_leading_paths() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), b"outside").unwrap();
        let index = repository.read_index().unwrap();
        let secret = ObjectId::compute(ObjectKind::Blob, b"top-secret\n");
        let mut entries = index.entries().to_vec();
        entries.push(
            IndexEntry::new("link/secret.txt", 0o100_644, secret, StatData::default()).unwrap(),
        );
        repository
            .write_index(&Index::new(index.version(), entries).unwrap())
            .unwrap();
        let removed = repository
            .remove(
                &["link/secret.txt"],
                &RemoveOptions {
                    cached: true,
                    ..RemoveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(removed, vec![b"link/secret.txt".to_vec()]);
        assert!(repository.read_index().unwrap().entries().is_empty());
        assert!(fs.exists(Path::new("repo/link")).unwrap());
    }
}
