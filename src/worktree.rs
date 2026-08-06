//! Worktree-to-index operations over the abstract filesystem.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::{
    EntryMode, Error, FileStat, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result,
    StatData, Tree, TreeEntry,
};

#[derive(Clone, Debug)]
pub struct CheckoutOptions {
    pub force: bool,
    pub max_object_size: usize,
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

    fn worktree_matches(&self, entry: &IndexEntry, full_path: &Path) -> Result<bool> {
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

    fn prune_empty_parents(&self, work_tree: &Path, mut parent: Option<&Path>) -> Result<()> {
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
        output: &mut Vec<IndexEntry>,
    ) -> Result<()> {
        let path = work_tree.join(relative);
        let metadata = self.filesystem().metadata(&path)?;
        if metadata.is_dir() {
            for child in self.filesystem().read_dir(&path)? {
                let child_relative = relative.join(child);
                if child_relative == Path::new(".git")
                    || work_tree.join(&child_relative) == self.git_dir()
                {
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn worktree_path(path: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut output = PathBuf::new();
    for component in path.split(|byte| *byte == b'/') {
        output.push(OsStr::from_bytes(component));
    }
    Ok(output)
}

#[cfg(not(unix))]
fn worktree_path(path: &[u8]) -> Result<PathBuf> {
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
}
