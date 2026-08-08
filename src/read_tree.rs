//! Populate and trivially merge the index from tree-ish objects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::{Error, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result, StatData};

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReadTreeOptions {
    pub empty: bool,
    pub merge: bool,
    pub reset: bool,
    pub dry_run: bool,
    pub aggressive: bool,
    /// Update affected worktree paths after a successful merge.
    pub update_worktree: bool,
    pub prefix: Option<Vec<u8>>,
    pub max_entries: usize,
    pub max_object_size: usize,
}

impl Default for ReadTreeOptions {
    fn default() -> Self {
        Self {
            empty: false,
            merge: false,
            reset: false,
            dry_run: false,
            aggressive: false,
            update_worktree: false,
            prefix: None,
            max_entries: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadTreeResult {
    pub entries: usize,
    pub conflicts: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Value {
    mode: u32,
    id: ObjectId,
}

impl Repository {
    /// Replace, bind, or trivially merge tree-ish objects into the index.
    ///
    /// # Errors
    /// Returns an error for contradictory modes, invalid trees, unresolved
    /// input, unsafe prefixes, nontrivial two-tree merges, or exceeded limits.
    pub fn read_tree_into_index(
        &self,
        trees: &[ObjectId],
        options: &ReadTreeOptions,
    ) -> Result<ReadTreeResult> {
        validate(trees, options)?;
        let current = self.read_index()?;
        if current.entries().iter().any(|entry| entry.stage() != 0)
            && (options.merge || options.prefix.is_some())
            && !options.reset
        {
            return read_error("read-tree requires a resolved index");
        }
        let maps = trees
            .iter()
            .map(|id| self.read_tree_entries(*id, options))
            .collect::<Result<Vec<_>>>()?;
        let (entries, conflicts) = if options.empty {
            (Vec::new(), Vec::new())
        } else if let Some(prefix) = &options.prefix {
            (bind(&current, &maps[0], prefix)?, Vec::new())
        } else if !options.merge && !options.reset {
            (to_entries(&maps[0], &BTreeMap::new(), 0)?, Vec::new())
        } else {
            match maps.as_slice() {
                [one] => (merge_one(&current, one)?, Vec::new()),
                [_, target] if options.reset => {
                    (to_entries(target, &BTreeMap::new(), 0)?, Vec::new())
                }
                [head, target] => (merge_two(&current, head, target)?, Vec::new()),
                [base, ours, theirs] => merge_three(base, ours, theirs, options.aggressive)?,
                _ => return read_error("merge requires one to three trees"),
            }
        };
        if entries.len() > options.max_entries {
            return read_error("read-tree entry count exceeds limit");
        }
        let result = ReadTreeResult {
            entries: entries.len(),
            conflicts,
        };
        let mut entries = entries;
        if options.update_worktree {
            self.update_read_tree_worktree(
                &current,
                &mut entries,
                &result.conflicts,
                options.reset,
                options.dry_run,
                options.max_object_size,
            )?;
        }
        if !options.dry_run {
            self.write_index(&current.with_entries(entries)?)?;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_lines)]
    fn update_read_tree_worktree(
        &self,
        current: &Index,
        desired: &mut [IndexEntry],
        conflicts: &[Vec<u8>],
        force: bool,
        dry_run: bool,
        max_object_size: usize,
    ) -> Result<()> {
        use crate::worktree::{index_stat, remove_worktree_tree, worktree_path};

        struct Write {
            path: Vec<u8>,
            full: PathBuf,
            mode: u32,
            id: ObjectId,
            data: Vec<u8>,
        }

        let root = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("read-tree -u requires a non-bare repository".into())
        })?;
        let old = current
            .entries()
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| (entry.path().to_vec(), entry))
            .collect::<BTreeMap<_, _>>();
        let next = desired
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| (entry.path().to_vec(), entry))
            .collect::<BTreeMap<_, _>>();
        let conflicted = conflicts.iter().cloned().collect::<BTreeSet<_>>();
        let changing = next
            .iter()
            .filter(|(path, entry)| {
                old.get(*path)
                    .is_none_or(|old| old.id() != entry.id() || old.mode() != entry.mode())
            })
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut removals = old
            .keys()
            .filter(|path| !next.contains_key(*path) && !conflicted.contains(*path))
            .cloned()
            .collect::<Vec<_>>();

        let mut rejected = Vec::new();
        if !force {
            for path in changing.iter().chain(removals.iter()) {
                if let Some(entry) = old.get(path) {
                    let full = root.join(worktree_path(path)?);
                    match self.filesystem().metadata(&full) {
                        Ok(_) if !self.worktree_matches(entry, &full)? => {
                            rejected.push(String::from_utf8_lossy(path).into_owned());
                        }
                        Ok(_) | Err(Error::NotFound(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }

        let mut writes = Vec::with_capacity(changing.len());
        let mut obstructing_parents = BTreeSet::new();
        for path in &changing {
            let entry = next[path];
            let relative = worktree_path(path)?;
            let full = root.join(&relative);
            if !force && !old.contains_key(path) && self.filesystem().exists(&full)? {
                rejected.push(String::from_utf8_lossy(path).into_owned());
            }
            for slash in path
                .iter()
                .enumerate()
                .filter_map(|(offset, byte)| (*byte == b'/').then_some(offset))
            {
                let parent_path = &path[..slash];
                let parent = root.join(worktree_path(parent_path)?);
                match self.filesystem().metadata(&parent) {
                    Ok(metadata) => {
                        let tracked_parent_will_be_removed = old.contains_key(parent_path)
                            && removals.iter().any(|path| path == parent_path);
                        if !metadata.is_dir() && !tracked_parent_will_be_removed && !force {
                            rejected.push(String::from_utf8_lossy(parent_path).into_owned());
                        } else if !metadata.is_dir() && !tracked_parent_will_be_removed {
                            obstructing_parents.insert(parent);
                        }
                    }
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            let data = if entry.mode() == 0o160_000 {
                Vec::new()
            } else {
                let object = self.read_object(entry.id(), max_object_size)?;
                if object.kind() != ObjectKind::Blob {
                    return Err(Error::InvalidTree(format!(
                        "entry `{}` points to the wrong object type",
                        String::from_utf8_lossy(path)
                    )));
                }
                object.data().to_vec()
            };
            writes.push(Write {
                path: path.clone(),
                full,
                mode: entry.mode(),
                id: entry.id(),
                data,
            });
        }
        rejected.sort_unstable();
        rejected.dedup();
        if !rejected.is_empty() {
            return Err(Error::CheckoutConflict(rejected));
        }
        let removals_set = removals.iter().cloned().collect::<BTreeSet<_>>();
        for path in &removals {
            let relative = worktree_path(path)?;
            if crate::worktree::has_symlink_leading_path(self.filesystem(), root, &relative)? {
                return Err(Error::BeyondSymbolicLink(relative));
            }
        }
        for parent in &obstructing_parents {
            let relative = parent
                .strip_prefix(root)
                .map_err(|_| Error::InvalidPath(parent.clone()))?;
            if crate::worktree::has_symlink_leading_path_replaced(
                self.filesystem(),
                root,
                relative,
                &removals_set,
            )? {
                return Err(Error::BeyondSymbolicLink(relative.to_path_buf()));
            }
        }
        if dry_run {
            return Ok(());
        }

        removals.sort_unstable_by_key(|path| std::cmp::Reverse(path.len()));
        for path in removals {
            let relative = worktree_path(&path)?;
            let full = root.join(&relative);
            match self.filesystem().metadata(&full) {
                Ok(metadata) if metadata.is_dir() => remove_worktree_tree(self, &full, force)?,
                Ok(_) => self.filesystem().remove_file(&full)?,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            self.prune_empty_parents(root, relative.parent())?;
        }
        for path in obstructing_parents {
            self.filesystem().remove_file(&path)?;
        }
        for write in writes {
            match self.filesystem().metadata(&write.full) {
                Ok(metadata) if metadata.is_dir() => {
                    remove_worktree_tree(self, &write.full, force)?;
                }
                Ok(_) => self.filesystem().remove_file(&write.full)?,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            if write.mode == 0o160_000 {
                self.filesystem().create_dir_all(&write.full)?;
            } else {
                if let Some(parent) = write.full.parent() {
                    self.filesystem().create_dir_all(parent)?;
                }
                match write.mode {
                    0o120_000 => self.filesystem().create_symlink(&write.full, &write.data)?,
                    0o100_644 | 0o100_755 => {
                        self.filesystem().write(&write.full, &write.data)?;
                        self.filesystem()
                            .set_executable(&write.full, write.mode == 0o100_755)?;
                    }
                    mode => return Err(Error::InvalidTree(format!("unsupported mode {mode:o}"))),
                }
            }
            let metadata = self.filesystem().metadata(&write.full)?;
            let replacement = IndexEntry::new(
                write.path.clone(),
                write.mode,
                write.id,
                index_stat(metadata.stat(), metadata.len()),
            )?;
            if let Some(slot) = desired
                .iter_mut()
                .find(|entry| entry.stage() == 0 && entry.path() == write.path)
            {
                *slot = replacement;
            }
        }
        Ok(())
    }

    fn read_tree_entries(
        &self,
        id: ObjectId,
        options: &ReadTreeOptions,
    ) -> Result<BTreeMap<Vec<u8>, Value>> {
        let object = self.read_object(id, options.max_object_size)?;
        let tree = match object.kind() {
            ObjectKind::Tree => id,
            ObjectKind::Commit => self.read_commit(id, options.max_object_size)?.tree(),
            ObjectKind::Tag => {
                let peeled = self.peel_tag(id, 64, options.max_object_size)?;
                match peeled.kind {
                    ObjectKind::Tree => peeled.id,
                    ObjectKind::Commit => {
                        self.read_commit(peeled.id, options.max_object_size)?.tree()
                    }
                    ObjectKind::Blob | ObjectKind::Tag => {
                        return read_error("object is not tree-ish");
                    }
                }
            }
            ObjectKind::Blob => return read_error("object is not tree-ish"),
        };
        let leaves = self.flattened_tree(tree, options.max_object_size)?;
        if leaves.len() > options.max_entries {
            return read_error("tree entry count exceeds limit");
        }
        Ok(leaves
            .into_iter()
            .map(|leaf| {
                (
                    leaf.path,
                    Value {
                        mode: leaf.raw_mode,
                        id: leaf.id,
                    },
                )
            })
            .collect())
    }
}

fn validate(trees: &[ObjectId], options: &ReadTreeOptions) -> Result<()> {
    let modes = usize::from(options.empty)
        + usize::from(options.merge)
        + usize::from(options.reset)
        + usize::from(options.prefix.is_some());
    if modes > 1 {
        return read_error("read-tree modes are mutually exclusive");
    }
    if options.empty != trees.is_empty() {
        return read_error("empty mode contradicts tree arguments");
    }
    if !options.empty && (trees.is_empty() || trees.len() > 3) {
        return read_error("read-tree requires one to three trees");
    }
    if !options.empty
        && !options.merge
        && !options.reset
        && options.prefix.is_none()
        && trees.len() != 1
    {
        return read_error("multiple trees require merge mode");
    }
    if options.prefix.is_some() && trees.len() != 1 {
        return read_error("prefix mode requires one tree");
    }
    if options.update_worktree && !(options.merge || options.reset || options.prefix.is_some()) {
        return read_error("worktree update requires merge, reset, or prefix mode");
    }
    if let Some(prefix) = &options.prefix
        && (prefix.is_empty()
            || !prefix.ends_with(b"/")
            || prefix.starts_with(b"/")
            || prefix.contains(&0)
            || prefix
                .split(|byte| *byte == b'/')
                .any(|part| matches!(part, b"." | b"..")))
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(prefix).into_owned().into(),
        ));
    }
    Ok(())
}

fn current(index: &Index) -> BTreeMap<Vec<u8>, (&IndexEntry, Value)> {
    index
        .entries()
        .iter()
        .filter(|entry| entry.stage() == 0)
        .map(|entry| {
            (
                entry.path().to_vec(),
                (
                    entry,
                    Value {
                        mode: entry.mode(),
                        id: entry.id(),
                    },
                ),
            )
        })
        .collect()
}

fn to_entries(
    map: &BTreeMap<Vec<u8>, Value>,
    stats: &BTreeMap<Vec<u8>, StatData>,
    stage: u8,
) -> Result<Vec<IndexEntry>> {
    map.iter()
        .map(|(path, value)| {
            IndexEntry::with_stage(
                path.clone(),
                value.mode,
                value.id,
                stats.get(path).copied().unwrap_or_default(),
                stage,
            )
        })
        .collect()
}

fn merge_one(index: &Index, tree: &BTreeMap<Vec<u8>, Value>) -> Result<Vec<IndexEntry>> {
    let old = current(index);
    let stats = tree
        .iter()
        .filter_map(|(path, value)| {
            old.get(path)
                .filter(|(_, cached)| cached == value)
                .map(|(entry, _)| (path.clone(), entry.stat()))
        })
        .collect();
    to_entries(tree, &stats, 0)
}

fn bind(index: &Index, tree: &BTreeMap<Vec<u8>, Value>, prefix: &[u8]) -> Result<Vec<IndexEntry>> {
    let mut output = index.entries().to_vec();
    for (path, value) in tree {
        let mut target = prefix.to_vec();
        target.extend_from_slice(path);
        if output.iter().any(|entry| overlaps(entry.path(), &target)) {
            return read_error("prefix tree collides with index");
        }
        output.push(IndexEntry::new(
            target,
            value.mode,
            value.id,
            StatData::default(),
        )?);
    }
    Ok(output)
}

fn merge_two(
    index: &Index,
    head: &BTreeMap<Vec<u8>, Value>,
    target: &BTreeMap<Vec<u8>, Value>,
) -> Result<Vec<IndexEntry>> {
    let cached = current(index);
    let paths = cached
        .keys()
        .chain(head.keys())
        .chain(target.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let initial = cached.is_empty();
    let mut output = Vec::new();
    for path in paths {
        let i = cached.get(&path).map(|(_, value)| *value);
        let h = head.get(&path).copied();
        let m = target.get(&path).copied();
        let result = match (i, h, m) {
            (None, None, value) => value,
            (None, Some(_), None) => None,
            (None, Some(left), Some(right)) if left == right => initial.then_some(right),
            (Some(value), None, None) => Some(value),
            (Some(value), None, Some(right)) if value == right => Some(value),
            (Some(value), Some(left), None) if value == left => None,
            (Some(value), Some(left), Some(right)) if left == right => Some(value),
            (Some(value), Some(_), Some(right)) if value == right => Some(value),
            (Some(value), Some(left), Some(right)) if value == left => Some(right),
            _ => return read_error("nontrivial two-tree merge"),
        };
        if let Some(value) = result {
            let stat = cached
                .get(&path)
                .filter(|(_, old)| *old == value)
                .map_or_else(StatData::default, |(entry, _)| entry.stat());
            output.push(IndexEntry::new(path, value.mode, value.id, stat)?);
        }
    }
    Ok(output)
}

fn merge_three(
    base: &BTreeMap<Vec<u8>, Value>,
    ours: &BTreeMap<Vec<u8>, Value>,
    theirs: &BTreeMap<Vec<u8>, Value>,
    aggressive: bool,
) -> Result<(Vec<IndexEntry>, Vec<Vec<u8>>)> {
    let paths = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut output = Vec::new();
    let mut conflicts = Vec::new();
    for path in paths {
        let b = base.get(&path).copied();
        let o = ours.get(&path).copied();
        let t = theirs.get(&path).copied();
        let resolved = if o == t {
            (o.is_some() || aggressive).then_some(o)
        } else if o == b && (t.is_some() || aggressive) {
            Some(t)
        } else if t == b && (o.is_some() || aggressive) {
            Some(o)
        } else {
            None
        };
        if let Some(value) = resolved {
            if let Some(value) = value {
                output.push(IndexEntry::new(
                    path,
                    value.mode,
                    value.id,
                    StatData::default(),
                )?);
            }
        } else {
            conflicts.push(path.clone());
            for (stage, value) in [(1, b), (2, o), (3, t)] {
                if let Some(value) = value {
                    output.push(IndexEntry::with_stage(
                        path.clone(),
                        value.mode,
                        value.id,
                        StatData::default(),
                        stage,
                    )?);
                }
            }
        }
    }
    Ok((output, conflicts))
}

fn overlaps(one: &[u8], two: &[u8]) -> bool {
    one == two
        || (one.starts_with(two) && one.get(two.len()) == Some(&b'/'))
        || (two.starts_with(one) && two.get(one.len()) == Some(&b'/'))
}

fn read_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(message.into()))
}

#[cfg(test)]
mod tests {
    use super::ReadTreeOptions;
    use crate::{
        Error, FileSystem, HostFileSystem, Index, IndexEntry, IndexVersion, InitOptions,
        MemoryFileSystem, ObjectId, ObjectKind, Repository, StatData,
    };
    use std::path::Path;

    #[test]
    fn replaces_empties_and_binds_prefixed_trees_atomically() {
        let repository = repository();
        let one = blob(&repository, b"one");
        let two = blob(&repository, b"two");
        let first = tree(&repository, &[("a", one)]);
        let second = tree(&repository, &[("b", two)]);
        repository
            .read_tree_into_index(&[first], &ReadTreeOptions::default())
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries()[0].path(), b"a");
        repository
            .read_tree_into_index(
                &[second],
                &ReadTreeOptions {
                    prefix: Some(b"sub/".to_vec()),
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .map(IndexEntry::path)
                .collect::<Vec<_>>(),
            [b"a".as_slice(), b"sub/b".as_slice()]
        );
        repository
            .read_tree_into_index(
                &[],
                &ReadTreeOptions {
                    empty: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert!(repository.read_index().unwrap().entries().is_empty());
    }

    #[test]
    fn two_tree_carries_compatible_index_changes_and_rejects_divergence() {
        let repository = repository();
        let old = blob(&repository, b"old");
        let new = blob(&repository, b"new");
        let local = blob(&repository, b"local");
        let head = tree(&repository, &[("a", old)]);
        let target = tree(&repository, &[("a", new)]);
        repository
            .read_tree_into_index(&[head], &ReadTreeOptions::default())
            .unwrap();
        repository
            .read_tree_into_index(
                &[head, target],
                &ReadTreeOptions {
                    merge: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), new);
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::new(b"a".to_vec(), 0o100_644, local, StatData::default())
                            .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        assert!(
            repository
                .read_tree_into_index(
                    &[head, target],
                    &ReadTreeOptions {
                        merge: true,
                        ..ReadTreeOptions::default()
                    },
                )
                .is_err()
        );
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), local);
    }

    #[test]
    fn three_tree_collapses_trivial_paths_and_records_conflict_stages() {
        let repository = repository();
        let base = blob(&repository, b"base");
        let ours = blob(&repository, b"ours");
        let theirs = blob(&repository, b"theirs");
        let common = blob(&repository, b"common");
        let changed = blob(&repository, b"changed");
        let base_tree = tree(&repository, &[("a", base), ("b", common)]);
        let ours_tree = tree(&repository, &[("a", ours), ("b", common)]);
        let theirs_tree = tree(&repository, &[("a", theirs), ("b", changed)]);
        let result = repository
            .read_tree_into_index(
                &[base_tree, ours_tree, theirs_tree],
                &ReadTreeOptions {
                    merge: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.conflicts, [b"a".to_vec()]);
        let index = repository.read_index().unwrap();
        assert_eq!(
            index
                .entries()
                .iter()
                .filter(|entry| entry.path() == b"a")
                .map(IndexEntry::stage)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let resolved = index
            .entries()
            .iter()
            .find(|entry| entry.path() == b"b")
            .unwrap();
        assert_eq!(resolved.stage(), 0);
        assert_eq!(resolved.id(), changed);
    }

    #[test]
    fn worktree_update_protects_data_but_reset_overwrites_and_dry_run_does_not() {
        let repository = worktree_repository();
        let old = blob(&repository, b"old");
        let new = blob(&repository, b"new");
        let final_blob = blob(&repository, b"final");
        let head = tree(&repository, &[("a", old)]);
        let target = tree(&repository, &[("a", new), ("b", new)]);
        let final_tree = tree(&repository, &[("a", final_blob)]);
        let nested_tree = tree(&repository, &[("dir/file", final_blob)]);
        repository
            .read_tree_into_index(&[head], &ReadTreeOptions::default())
            .unwrap();
        repository.filesystem().write("a".as_ref(), b"old").unwrap();
        repository
            .filesystem()
            .write("b".as_ref(), b"untracked")
            .unwrap();

        let protected = ReadTreeOptions {
            merge: true,
            update_worktree: true,
            ..ReadTreeOptions::default()
        };
        assert!(
            repository
                .read_tree_into_index(&[head, target], &protected)
                .is_err()
        );
        assert_eq!(repository.filesystem().read("a".as_ref()).unwrap(), b"old");
        assert_eq!(
            repository.filesystem().read("b".as_ref()).unwrap(),
            b"untracked"
        );
        assert_eq!(repository.read_index().unwrap().entries().len(), 1);

        repository
            .read_tree_into_index(
                &[target],
                &ReadTreeOptions {
                    reset: true,
                    update_worktree: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(repository.filesystem().read("a".as_ref()).unwrap(), b"new");
        assert_eq!(repository.filesystem().read("b".as_ref()).unwrap(), b"new");

        repository
            .filesystem()
            .write("a".as_ref(), b"local")
            .unwrap();
        assert!(
            repository
                .read_tree_into_index(
                    &[target, final_tree],
                    &ReadTreeOptions {
                        merge: true,
                        update_worktree: true,
                        ..ReadTreeOptions::default()
                    },
                )
                .is_err()
        );
        assert_eq!(
            repository.filesystem().read("a".as_ref()).unwrap(),
            b"local"
        );
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), new);

        repository
            .read_tree_into_index(
                &[final_tree],
                &ReadTreeOptions {
                    reset: true,
                    update_worktree: true,
                    dry_run: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            repository.filesystem().read("a".as_ref()).unwrap(),
            b"local"
        );
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), new);

        repository
            .filesystem()
            .write("dir".as_ref(), b"untracked obstruction")
            .unwrap();
        repository
            .read_tree_into_index(
                &[nested_tree],
                &ReadTreeOptions {
                    reset: true,
                    update_worktree: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            repository.filesystem().read("dir/file".as_ref()).unwrap(),
            b"final"
        );
    }

    #[test]
    fn unresolved_merge_keeps_worktree_file_and_publishes_stages() {
        let repository = worktree_repository();
        let base = blob(&repository, b"base");
        let ours = blob(&repository, b"ours");
        let theirs = blob(&repository, b"theirs");
        let base_tree = tree(&repository, &[("a", base)]);
        let ours_tree = tree(&repository, &[("a", ours)]);
        let theirs_tree = tree(&repository, &[("a", theirs)]);
        repository
            .read_tree_into_index(&[ours_tree], &ReadTreeOptions::default())
            .unwrap();
        repository
            .filesystem()
            .write("a".as_ref(), b"ours")
            .unwrap();
        let result = repository
            .read_tree_into_index(
                &[base_tree, ours_tree, theirs_tree],
                &ReadTreeOptions {
                    merge: true,
                    update_worktree: true,
                    ..ReadTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.conflicts, [b"a".to_vec()]);
        assert_eq!(repository.filesystem().read("a".as_ref()).unwrap(), b"ours");
        assert_eq!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .map(IndexEntry::stage)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn refuses_update_worktree_removal_through_a_symlinked_directory() {
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
        let empty = repository
            .write_index_tree(&Index::new(IndexVersion::V2, Vec::new()).unwrap())
            .unwrap();
        assert!(matches!(
            repository.read_tree_into_index(
                &[empty],
                &ReadTreeOptions {
                    update_worktree: true,
                    reset: true,
                    ..ReadTreeOptions::default()
                }
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(
            outside.join("secret.txt").exists(),
            "read-tree -u must not delete through a symlinked directory"
        );
    }

    fn repository() -> Repository {
        Repository::init(
            MemoryFileSystem::new(),
            ".",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap()
    }

    fn worktree_repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), ".", &InitOptions::default()).unwrap()
    }

    fn blob(repository: &Repository, data: &[u8]) -> crate::ObjectId {
        repository.write_object(ObjectKind::Blob, data).unwrap()
    }

    fn tree(repository: &Repository, entries: &[(&str, crate::ObjectId)]) -> crate::ObjectId {
        repository
            .write_index_tree(
                &Index::new(
                    IndexVersion::V2,
                    entries
                        .iter()
                        .map(|(path, id)| {
                            IndexEntry::new(
                                path.as_bytes().to_vec(),
                                0o100_644,
                                *id,
                                StatData::default(),
                            )
                            .unwrap()
                        })
                        .collect(),
                )
                .unwrap(),
            )
            .unwrap()
    }
}
