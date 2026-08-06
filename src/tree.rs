//! Canonical Git tree objects.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::{Error, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EntryMode {
    Blob,
    BlobExecutable,
    Link,
    Tree,
    Gitlink,
}

impl EntryMode {
    #[must_use]
    pub const fn as_octal(self) -> &'static [u8] {
        match self {
            Self::Blob => b"100644",
            Self::BlobExecutable => b"100755",
            Self::Link => b"120000",
            Self::Tree => b"40000",
            Self::Gitlink => b"160000",
        }
    }

    #[must_use]
    pub const fn object_kind(self) -> ObjectKind {
        match self {
            Self::Tree => ObjectKind::Tree,
            Self::Gitlink => ObjectKind::Commit,
            Self::Blob | Self::BlobExecutable | Self::Link => ObjectKind::Blob,
        }
    }

    const fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }

    fn parse(value: &[u8]) -> Result<Self> {
        let raw = std::str::from_utf8(value)
            .ok()
            .and_then(|value| u32::from_str_radix(value, 8).ok())
            .ok_or_else(|| {
                Error::InvalidTree(format!(
                    "unsupported mode `{}`",
                    String::from_utf8_lossy(value)
                ))
            })?;
        match raw & 0o170_000 {
            0o100_000 if raw & 0o111 == 0 => Ok(Self::Blob),
            0o100_000 => Ok(Self::BlobExecutable),
            0o120_000 => Ok(Self::Link),
            0o040_000 => Ok(Self::Tree),
            0o160_000 => Ok(Self::Gitlink),
            _ => Err(Error::InvalidTree(format!("unsupported mode `{raw:o}`"))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    mode: EntryMode,
    name: Vec<u8>,
    id: ObjectId,
}

impl TreeEntry {
    /// Create a tree entry with a byte-preserving filename.
    ///
    /// # Errors
    /// Returns an error for empty names or names containing `/` or NUL.
    pub fn new(mode: EntryMode, name: impl Into<Vec<u8>>, id: ObjectId) -> Result<Self> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self { mode, name, id })
    }

    #[must_use]
    pub const fn mode(&self) -> EntryMode {
        self.mode
    }

    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Build a tree, sorting entries with Git's directory-aware byte ordering.
    ///
    /// # Errors
    /// Returns an error when duplicate filenames are supplied.
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        let mut names = BTreeSet::new();
        if entries
            .iter()
            .any(|entry| !names.insert(entry.name.clone()))
        {
            return Err(Error::InvalidTree("duplicate filename".into()));
        }
        entries.sort_unstable_by(compare_entries);
        Ok(Self { entries })
    }

    #[must_use]
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Parse a canonical SHA-1 tree body.
    ///
    /// # Errors
    /// Returns an error for truncated, malformed, duplicate, or incorrectly
    /// ordered entries.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut cursor = 0;
        while cursor < data.len() {
            let space = data[cursor..]
                .iter()
                .position(|byte| *byte == b' ')
                .map(|index| cursor + index)
                .ok_or_else(|| Error::InvalidTree("entry has no mode separator".into()))?;
            let mode = EntryMode::parse(&data[cursor..space])?;
            let name_start = space + 1;
            let nul = data[name_start..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|index| name_start + index)
                .ok_or_else(|| Error::InvalidTree("entry has no name terminator".into()))?;
            let id_start = nul + 1;
            let id_end = id_start + ObjectId::LENGTH;
            if id_end > data.len() {
                return Err(Error::InvalidTree("truncated object ID".into()));
            }
            let mut bytes = [0; ObjectId::LENGTH];
            bytes.copy_from_slice(&data[id_start..id_end]);
            entries.push(TreeEntry::new(
                mode,
                data[name_start..nul].to_vec(),
                ObjectId::from_bytes(bytes),
            )?);
            cursor = id_end;
        }
        let tree = Self::new(entries.clone())?;
        if tree.entries != entries {
            return Err(Error::InvalidTree(
                "entries are not canonically ordered".into(),
            ));
        }
        Ok(tree)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let capacity = self
            .entries
            .iter()
            .map(|entry| entry.mode.as_octal().len() + 1 + entry.name.len() + 1 + ObjectId::LENGTH)
            .sum();
        let mut data = Vec::with_capacity(capacity);
        for entry in &self.entries {
            data.extend_from_slice(entry.mode.as_octal());
            data.push(b' ');
            data.extend_from_slice(&entry.name);
            data.push(0);
            data.extend_from_slice(entry.id.as_bytes());
        }
        data
    }
}

impl Repository {
    /// Store a canonical tree object.
    ///
    /// # Errors
    /// Returns an error when object storage fails.
    pub fn write_tree(&self, tree: &Tree) -> Result<ObjectId> {
        self.write_object(ObjectKind::Tree, &tree.encode())
    }

    /// Read and parse a loose tree object.
    ///
    /// # Errors
    /// Returns an error for a missing, corrupt, oversized, non-tree, or
    /// malformed object.
    pub fn read_tree(&self, id: ObjectId, max_size: usize) -> Result<Tree> {
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Tree {
            return Err(Error::InvalidTree(format!("object {id} is not a tree")));
        }
        Tree::parse(object.data())
    }
}

fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
        return Err(Error::InvalidTree(
            "filename is empty or contains `/` or NUL".into(),
        ));
    }
    Ok(())
}

fn compare_entries(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    let common = left.name.len().min(right.name.len());
    match left.name[..common].cmp(&right.name[..common]) {
        Ordering::Equal => {
            let left_next = left
                .name
                .get(common)
                .copied()
                .unwrap_or(if left.mode.is_tree() { b'/' } else { 0 });
            let right_next = right
                .name
                .get(common)
                .copied()
                .unwrap_or(if right.mode.is_tree() { b'/' } else { 0 });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{InitOptions, MemoryFileSystem};

    fn id(value: &str) -> ObjectId {
        ObjectId::from_str(value).unwrap()
    }

    #[test]
    fn sorts_like_git_base_name_compare() {
        let object = id("1111111111111111111111111111111111111111");
        let tree = Tree::new(vec![
            TreeEntry::new(EntryMode::Tree, b"foo".to_vec(), object).unwrap(),
            TreeEntry::new(EntryMode::Blob, b"foo.bar".to_vec(), object).unwrap(),
            TreeEntry::new(EntryMode::Blob, b"foo".to_vec(), object).unwrap(),
        ]);
        assert!(tree.is_err(), "file and directory cannot share a tree name");

        let tree = Tree::new(vec![
            TreeEntry::new(EntryMode::Tree, b"foo".to_vec(), object).unwrap(),
            TreeEntry::new(EntryMode::Blob, b"foo.bar".to_vec(), object).unwrap(),
            TreeEntry::new(EntryMode::Blob, b"foo0".to_vec(), object).unwrap(),
        ])
        .unwrap();
        assert_eq!(
            tree.entries()
                .iter()
                .map(TreeEntry::name)
                .collect::<Vec<_>>(),
            [b"foo.bar".as_slice(), b"foo".as_slice(), b"foo0".as_slice()]
        );
    }

    #[test]
    fn round_trips_all_canonical_modes_and_non_utf8_names() {
        let object = id("0123456789012345678901234567890123456789");
        let modes = [
            EntryMode::Blob,
            EntryMode::BlobExecutable,
            EntryMode::Link,
            EntryMode::Tree,
            EntryMode::Gitlink,
        ];
        let entries = modes
            .into_iter()
            .enumerate()
            .map(|(index, mode)| {
                TreeEntry::new(
                    mode,
                    vec![b'a' + u8::try_from(index).unwrap(), 0xff],
                    object,
                )
                .unwrap()
            })
            .collect();
        let tree = Tree::new(entries).unwrap();
        assert_eq!(Tree::parse(&tree.encode()).unwrap(), tree);
    }

    #[test]
    fn writes_known_git_empty_tree_and_reads_it() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = Tree::default();
        let id = repository.write_tree(&tree).unwrap();
        assert_eq!(id.to_string(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(repository.read_tree(id, 1024).unwrap(), tree);
    }

    #[test]
    fn rejects_noncanonical_order_and_truncation() {
        let object = id("1111111111111111111111111111111111111111");
        let a = TreeEntry::new(EntryMode::Blob, b"a".to_vec(), object).unwrap();
        let b = TreeEntry::new(EntryMode::Blob, b"b".to_vec(), object).unwrap();
        let mut reversed = Vec::new();
        for entry in [&b, &a] {
            reversed.extend_from_slice(entry.mode.as_octal());
            reversed.push(b' ');
            reversed.extend_from_slice(entry.name());
            reversed.push(0);
            reversed.extend_from_slice(entry.id().as_bytes());
        }
        assert!(Tree::parse(&reversed).is_err());
        reversed.pop();
        assert!(Tree::parse(&reversed).is_err());
    }
}
