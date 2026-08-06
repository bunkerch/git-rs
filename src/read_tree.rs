//! Populate and trivially merge the index from tree-ish objects.

use std::collections::{BTreeMap, BTreeSet};

use crate::{Error, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result, StatData};

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReadTreeOptions {
    pub empty: bool,
    pub merge: bool,
    pub reset: bool,
    pub dry_run: bool,
    pub aggressive: bool,
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
        if !options.dry_run {
            self.write_index(&current.with_entries(entries)?)?;
        }
        Ok(result)
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
        Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, ObjectKind, Repository,
        StatData,
    };

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
                .map(|entry| entry.path())
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
                .map(|entry| entry.stage())
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
