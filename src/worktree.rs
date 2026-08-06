//! Worktree-to-index operations over the abstract filesystem.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::{
    EntryMode, Error, FileStat, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result,
    StatData, Tree, TreeEntry,
};

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
        let relative = normalize_relative(path.as_ref())?;
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("cannot add from a bare repository".into()))?;
        let mut added = Vec::new();
        let target = work_tree.join(&relative);
        match self.filesystem().metadata(&target) {
            Ok(_) => self.collect_entries(work_tree, &relative, &mut added)?,
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let existing = self.read_index()?;
        let prefix = index_path(&relative)?;
        let mut entries = existing.entries().to_vec();
        entries.retain(|entry| !path_is_selected(entry.path(), &prefix));
        let count = added.len();
        entries.extend(added);
        self.write_index(&Index::new(existing.version(), entries)?)?;
        Ok(count)
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
        output: &mut Vec<IndexEntry>,
    ) -> Result<()> {
        let path = work_tree.join(relative);
        let metadata = self.filesystem().metadata(&path)?;
        if metadata.is_dir() {
            for child in self.filesystem().read_dir(&path)? {
                let child_relative = relative.join(child);
                if work_tree.join(&child_relative) == self.git_dir() {
                    continue;
                }
                self.collect_entries(work_tree, &child_relative, output)?;
            }
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
            index_path(relative)?,
            mode,
            id,
            index_stat(metadata.stat(), metadata.len()),
        )?);
        Ok(())
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

fn index_stat(stat: FileStat, len: u64) -> StatData {
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
    use crate::{FileSystem, InitOptions, MemoryFileSystem};

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
    fn never_adds_repository_metadata() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        assert_eq!(repository.add(".").unwrap(), 0);
        assert!(repository.read_index().unwrap().entries().is_empty());
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
}
