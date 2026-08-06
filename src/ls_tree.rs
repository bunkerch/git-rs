//! Ordered, byte-preserving tree inspection.

use crate::{
    EntryMode, Error, ObjectId, ObjectKind, Repository, Result, RevisionOptions, TreeEntry,
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsTreeOptions {
    pub recursive: bool,
    pub trees_only: bool,
    pub show_trees: bool,
    pub include_object_size: bool,
    /// Literal, repository-root-relative byte paths.
    pub paths: Vec<Vec<u8>>,
    pub max_entries: usize,
    pub max_depth: usize,
    pub revision: RevisionOptions,
}

impl Default for LsTreeOptions {
    fn default() -> Self {
        Self {
            recursive: false,
            trees_only: false,
            show_trees: false,
            include_object_size: false,
            paths: Vec::new(),
            max_entries: 10_000_000,
            max_depth: 4096,
            revision: RevisionOptions::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsTreeEntry {
    path: Vec<u8>,
    mode: EntryMode,
    kind: ObjectKind,
    id: ObjectId,
    object_size: Option<u64>,
}

impl LsTreeEntry {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    #[must_use]
    pub const fn mode(&self) -> EntryMode {
        self.mode
    }

    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }

    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    /// Blob size when `include_object_size` was requested; trees and gitlinks
    /// have no ls-tree object size.
    #[must_use]
    pub const fn object_size(&self) -> Option<u64> {
        self.object_size
    }

    #[must_use]
    pub const fn mode_number(&self) -> u32 {
        match self.mode {
            EntryMode::Blob => 0o100_644,
            EntryMode::BlobExecutable => 0o100_755,
            EntryMode::Link => 0o120_000,
            EntryMode::Tree => 0o040_000,
            EntryMode::Gitlink => 0o160_000,
        }
    }
}

impl Repository {
    /// List a tree-ish in canonical Git tree order.
    ///
    /// Literal nested paths cause targeted descent even without `recursive`.
    /// With recursive traversal, directory entries are hidden unless
    /// `show_trees` is set; `trees_only && recursive` implies `show_trees`, as
    /// in Git. Filenames remain arbitrary non-NUL bytes.
    ///
    /// # Errors
    /// Returns an error for an invalid/non-tree-ish revision, unsafe path,
    /// corrupt/wrongly-typed objects, exceeded entry/depth/size limits, or
    /// storage failures.
    pub fn ls_tree(&self, treeish: &str, options: &LsTreeOptions) -> Result<Vec<LsTreeEntry>> {
        validate_paths(&options.paths)?;
        let resolved = self.resolve_revision(treeish, &options.revision)?;
        let tree = match resolved.kind {
            ObjectKind::Tree => resolved.id,
            ObjectKind::Commit => self
                .read_commit(resolved.id, options.revision.max_object_size)?
                .tree(),
            ObjectKind::Tag => {
                self.resolve_revision(&format!("{treeish}^{{tree}}"), &options.revision)?
                    .id
            }
            ObjectKind::Blob => {
                return Err(Error::InvalidTree(format!(
                    "{treeish} does not resolve to a tree-ish"
                )));
            }
        };
        let mut output = Vec::new();
        let mut visited = 0;
        self.walk_ls_tree(tree, &mut Vec::new(), 0, options, &mut visited, &mut output)?;
        Ok(output)
    }

    fn walk_ls_tree(
        &self,
        tree: ObjectId,
        prefix: &mut Vec<u8>,
        depth: usize,
        options: &LsTreeOptions,
        visited: &mut usize,
        output: &mut Vec<LsTreeEntry>,
    ) -> Result<()> {
        if depth > options.max_depth {
            return Err(Error::InvalidTree("ls-tree exceeds depth limit".into()));
        }
        for entry in self
            .read_tree(tree, options.revision.max_object_size)?
            .entries()
        {
            *visited = visited
                .checked_add(1)
                .ok_or_else(|| Error::InvalidTree("ls-tree entry count overflow".into()))?;
            if *visited > options.max_entries {
                return Err(Error::InvalidTree("ls-tree exceeds entry limit".into()));
            }
            let old_len = prefix.len();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(entry.name());
            let selected = path_selected(prefix, &options.paths);
            let descend = entry.mode() == EntryMode::Tree
                && should_descend(prefix, options.recursive, &options.paths);
            let show_trees = options.show_trees || (options.trees_only && options.recursive);
            let emit = (selected || (entry.mode() == EntryMode::Tree && descend && show_trees))
                && !(options.trees_only && entry.mode() != EntryMode::Tree)
                && !(descend && entry.mode() == EntryMode::Tree && !show_trees);
            if emit {
                output.push(self.ls_tree_entry(prefix, entry, options)?);
            }
            if descend {
                self.walk_ls_tree(entry.id(), prefix, depth + 1, options, visited, output)?;
            }
            prefix.truncate(old_len);
        }
        Ok(())
    }

    fn ls_tree_entry(
        &self,
        path: &[u8],
        entry: &TreeEntry,
        options: &LsTreeOptions,
    ) -> Result<LsTreeEntry> {
        let kind = entry.mode().object_kind();
        let object_size = if options.include_object_size
            && matches!(
                entry.mode(),
                EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link
            ) {
            let object = self.read_object(entry.id(), options.revision.max_object_size)?;
            if object.kind() != ObjectKind::Blob {
                return Err(Error::InvalidTree(format!(
                    "entry `{}` does not point to a blob",
                    String::from_utf8_lossy(path)
                )));
            }
            Some(
                u64::try_from(object.data().len()).map_err(|_| {
                    Error::InvalidTree("blob size cannot be represented as u64".into())
                })?,
            )
        } else {
            None
        };
        Ok(LsTreeEntry {
            path: path.to_vec(),
            mode: entry.mode(),
            kind,
            id: entry.id(),
            object_size,
        })
    }
}

fn validate_paths(paths: &[Vec<u8>]) -> Result<()> {
    for path in paths {
        if path.is_empty()
            || path.starts_with(b"/")
            || path.ends_with(b"/")
            || path.contains(&0)
            || path
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || matches!(component, b"." | b".."))
        {
            return Err(Error::InvalidPath(
                String::from_utf8_lossy(path).into_owned().into(),
            ));
        }
    }
    Ok(())
}

fn path_selected(path: &[u8], specs: &[Vec<u8>]) -> bool {
    specs.is_empty()
        || specs
            .iter()
            .any(|spec| spec == path || path_below(path, spec))
}

fn should_descend(path: &[u8], recursive: bool, specs: &[Vec<u8>]) -> bool {
    if specs.is_empty() {
        return recursive;
    }
    specs.iter().any(|spec| {
        path_below(spec, path) || (recursive && (spec == path || path_below(path, spec)))
    })
}

fn path_below(path: &[u8], parent: &[u8]) -> bool {
    path.len() > parent.len() && path.starts_with(parent) && path.get(parent.len()) == Some(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitBuilder, InitOptions, MemoryFileSystem, Signature, Tree};

    fn fixture() -> Repository {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"hello").unwrap();
        let executable = repository.write_object(ObjectKind::Blob, b"run").unwrap();
        let nested = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                    TreeEntry::new(EntryMode::BlobExecutable, vec![0xff], executable).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let root = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"dir".to_vec(), nested).unwrap(),
                    TreeEntry::new(EntryMode::Link, b"link".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("A U Thor", "author@example.com", 1, 0).unwrap();
        let commit = repository
            .write_commit(
                &CommitBuilder::new(root, signature.clone(), signature)
                    .message(b"tree\n")
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &crate::ReferenceName::branch("main").unwrap(),
                commit,
                crate::PreviousValue::MustNotExist,
            )
            .unwrap();
        repository
    }

    #[test]
    fn lists_default_recursive_tree_only_and_sizes_in_git_order() {
        let repository = fixture();
        let top = repository
            .ls_tree("main", &LsTreeOptions::default())
            .unwrap();
        assert_eq!(
            top.iter().map(LsTreeEntry::path).collect::<Vec<_>>(),
            vec![b"dir".as_slice(), b"link".as_slice()]
        );
        let recursive = repository
            .ls_tree(
                "main",
                &LsTreeOptions {
                    recursive: true,
                    include_object_size: true,
                    ..LsTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            recursive.iter().map(LsTreeEntry::path).collect::<Vec<_>>(),
            vec![
                b"dir/file".as_slice(),
                b"dir/\xff".as_slice(),
                b"link".as_slice()
            ]
        );
        assert_eq!(recursive[0].object_size(), Some(5));
        let trees = repository
            .ls_tree(
                "main",
                &LsTreeOptions {
                    recursive: true,
                    trees_only: true,
                    ..LsTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].path(), b"dir");
    }

    #[test]
    fn literal_nested_path_descends_without_global_recursion_and_limits_work() {
        let repository = fixture();
        let selected = repository
            .ls_tree(
                "main",
                &LsTreeOptions {
                    paths: vec![b"dir/file".to_vec()],
                    ..LsTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path(), b"dir/file");
        let with_ancestor = repository
            .ls_tree(
                "main",
                &LsTreeOptions {
                    show_trees: true,
                    paths: vec![b"dir/file".to_vec()],
                    ..LsTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            with_ancestor
                .iter()
                .map(LsTreeEntry::path)
                .collect::<Vec<_>>(),
            vec![b"dir".as_slice(), b"dir/file".as_slice()]
        );
        assert!(
            repository
                .ls_tree(
                    "main",
                    &LsTreeOptions {
                        recursive: true,
                        max_entries: 1,
                        ..LsTreeOptions::default()
                    }
                )
                .is_err()
        );
    }
}
