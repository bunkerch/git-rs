//! Materialize the current index as canonical Git tree objects.

use crate::{Error, Index, IndexEntry, ObjectId, Repository, Result};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WriteTreeOptions {
    /// Permit index entries whose referenced objects are absent.
    pub missing_ok: bool,
    /// Select one indexed directory, with or without a trailing slash.
    pub prefix: Option<Vec<u8>>,
}

impl Repository {
    /// Write the repository's current index as a tree hierarchy.
    ///
    /// # Errors
    /// Returns an error for an unmerged index, missing objects unless allowed,
    /// an absent or unsafe prefix, invalid entries, or object-storage failure.
    pub fn write_current_index_tree(&self, options: &WriteTreeOptions) -> Result<ObjectId> {
        self.write_index_tree_with_options(&self.read_index()?, options)
    }

    /// Write an explicitly supplied index as a tree hierarchy.
    ///
    /// Intent-to-add entries are omitted, matching Git's cache-tree builder.
    /// Gitlinks need not exist in the superproject object database.
    ///
    /// # Errors
    /// Returns the errors documented by [`Self::write_current_index_tree`].
    pub fn write_index_tree_with_options(
        &self,
        index: &Index,
        options: &WriteTreeOptions,
    ) -> Result<ObjectId> {
        if let Some(entry) = index.entries().iter().find(|entry| entry.stage() != 0) {
            return Err(Error::InvalidTree(format!(
                "unmerged index entry `{}`",
                String::from_utf8_lossy(entry.path())
            )));
        }
        let prefix = options
            .prefix
            .as_deref()
            .map(normalize_prefix)
            .transpose()?;
        if let Some(prefix) = prefix.as_deref()
            && let Some(entry) = index.entries().iter().find(|entry| entry.path() == prefix)
        {
            if entry.mode() != 0o040_000 {
                return Err(prefix_not_found(prefix));
            }
            if !options.missing_ok && !self.contains_object(entry.id())? {
                return Err(invalid_object(entry));
            }
            return Ok(entry.id());
        }
        let selected = select_entries(index, prefix.as_deref())?;
        let mut entries = Vec::with_capacity(selected.len());
        for entry in selected {
            if entry.intent_to_add() {
                continue;
            }
            if !options.missing_ok
                && entry.mode() != 0o160_000
                && !self.contains_object(entry.id())?
            {
                return Err(invalid_object(&entry));
            }
            entries.push(entry);
        }
        self.write_index_tree(&Index::new(index.version(), entries)?)
    }
}

fn normalize_prefix(prefix: &[u8]) -> Result<Vec<u8>> {
    let prefix = prefix.strip_suffix(b"/").unwrap_or(prefix);
    if prefix.is_empty()
        || prefix.starts_with(b"/")
        || prefix.contains(&0)
        || prefix
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || matches!(part, b"." | b".."))
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(prefix).into_owned().into(),
        ));
    }
    Ok(prefix.to_vec())
}

fn select_entries(index: &Index, prefix: Option<&[u8]>) -> Result<Vec<IndexEntry>> {
    let Some(prefix) = prefix else {
        return Ok(index.entries().to_vec());
    };
    let mut directory = prefix.to_vec();
    directory.push(b'/');
    let entries = index
        .entries()
        .iter()
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(directory.as_slice())
                .map(|path| entry.clone().with_path(path.to_vec()))
        })
        .collect::<Result<Vec<_>>>()?;
    if entries.is_empty() {
        return Err(prefix_not_found(prefix));
    }
    Ok(entries)
}

fn invalid_object(entry: &IndexEntry) -> Error {
    Error::InvalidTree(format!(
        "invalid object {} for `{}`",
        entry.id(),
        String::from_utf8_lossy(entry.path())
    ))
}

fn prefix_not_found(prefix: &[u8]) -> Error {
    Error::InvalidTree(format!(
        "prefix `{}` not found",
        String::from_utf8_lossy(prefix)
    ))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::WriteTreeOptions;
    use crate::{
        Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, ObjectId, ObjectKind,
        Repository, StatData,
    };

    #[test]
    fn writes_root_and_prefixed_trees_with_native_intent_to_add_semantics() {
        let repository = repository();
        let a = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let b = repository.write_object(ObjectKind::Blob, b"b").unwrap();
        let index = Index::new(
            IndexVersion::V3,
            vec![
                entry(b"root", a),
                entry(b"sub/a", a),
                entry(b"sub/b", b).with_intent_to_add(true),
            ],
        )
        .unwrap();
        let root = repository
            .write_index_tree_with_options(&index, &WriteTreeOptions::default())
            .unwrap();
        let prefix = repository
            .write_index_tree_with_options(
                &index,
                &WriteTreeOptions {
                    prefix: Some(b"sub".to_vec()),
                    ..WriteTreeOptions::default()
                },
            )
            .unwrap();
        assert_ne!(root, prefix);
        let tree = repository.read_tree(prefix, 1024).unwrap();
        assert_eq!(tree.entries().len(), 1);
        assert_eq!(tree.entries()[0].name(), b"a");
        assert_eq!(tree.entries()[0].id(), a);
    }

    #[test]
    fn validates_missing_objects_except_gitlinks_and_missing_ok() {
        let repository = repository();
        let missing = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        let blob_index = Index::new(IndexVersion::V2, vec![entry(b"a", missing)]).unwrap();
        assert!(
            repository
                .write_index_tree_with_options(&blob_index, &WriteTreeOptions::default())
                .is_err()
        );
        assert!(
            repository
                .write_index_tree_with_options(
                    &blob_index,
                    &WriteTreeOptions {
                        missing_ok: true,
                        ..WriteTreeOptions::default()
                    }
                )
                .is_ok()
        );
        let gitlink = Index::new(
            IndexVersion::V2,
            vec![
                IndexEntry::new(b"sub".to_vec(), 0o160_000, missing, StatData::default()).unwrap(),
            ],
        )
        .unwrap();
        assert!(
            repository
                .write_index_tree_with_options(&gitlink, &WriteTreeOptions::default())
                .is_ok()
        );
    }

    #[test]
    fn rejects_unmerged_indexes_and_missing_or_unsafe_prefixes() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let unmerged = Index::new(
            IndexVersion::V2,
            vec![
                IndexEntry::with_stage(b"a".to_vec(), 0o100_644, blob, StatData::default(), 2)
                    .unwrap(),
            ],
        )
        .unwrap();
        assert!(
            repository
                .write_index_tree_with_options(&unmerged, &WriteTreeOptions::default())
                .is_err()
        );
        let index = Index::new(IndexVersion::V2, vec![entry(b"sub/a", blob)]).unwrap();
        for prefix in [b"missing".as_slice(), b"../sub", b"sub//nested"] {
            assert!(
                repository
                    .write_index_tree_with_options(
                        &index,
                        &WriteTreeOptions {
                            prefix: Some(prefix.to_vec()),
                            ..WriteTreeOptions::default()
                        }
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn returns_an_existing_sparse_directory_tree_for_its_exact_prefix() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let subtree = repository
            .write_index_tree(&Index::new(IndexVersion::V2, vec![entry(b"a", blob)]).unwrap())
            .unwrap();
        let sparse = Index::new(
            IndexVersion::V3,
            vec![
                IndexEntry::new(b"sub".to_vec(), 0o040_000, subtree, StatData::default())
                    .unwrap()
                    .with_skip_worktree(true),
            ],
        )
        .unwrap();
        assert_eq!(
            repository
                .write_index_tree_with_options(
                    &sparse,
                    &WriteTreeOptions {
                        prefix: Some(b"sub/".to_vec()),
                        ..WriteTreeOptions::default()
                    }
                )
                .unwrap(),
            subtree
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

    fn entry(path: &[u8], id: ObjectId) -> IndexEntry {
        IndexEntry::new(path.to_vec(), 0o100_644, id, StatData::default()).unwrap()
    }
}
