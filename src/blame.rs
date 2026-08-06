//! Bounded line attribution across commit history.

use std::collections::{BTreeMap, VecDeque};

use crate::{
    EntryMode, Error, ObjectId, ObjectKind, Repository, Result, RevisionOptions, Signature,
};

/// Traversal, object, and diff bounds for blame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameOptions {
    pub first_parent: bool,
    pub max_commits: usize,
    pub max_tree_entries: usize,
    pub max_lines: usize,
    pub max_trace_cells: usize,
    pub revision: RevisionOptions,
}

impl Default for BlameOptions {
    fn default() -> Self {
        Self {
            first_parent: false,
            max_commits: 1_000_000,
            max_tree_entries: 10_000_000,
            max_lines: 1_000_000,
            max_trace_cells: 10_000_000,
            revision: RevisionOptions::default(),
        }
    }
}

/// Attribution for one final file line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameLine {
    commit: ObjectId,
    author: Signature,
    original_path: Vec<u8>,
    original_line: usize,
    final_line: usize,
    boundary: bool,
    contents: Vec<u8>,
}

impl BlameLine {
    #[must_use]
    pub const fn commit(&self) -> ObjectId {
        self.commit
    }
    #[must_use]
    pub const fn author(&self) -> &Signature {
        &self.author
    }
    #[must_use]
    pub fn original_path(&self) -> &[u8] {
        &self.original_path
    }
    #[must_use]
    pub const fn original_line(&self) -> usize {
        self.original_line
    }
    #[must_use]
    pub const fn final_line(&self) -> usize {
        self.final_line
    }
    #[must_use]
    pub const fn is_boundary(&self) -> bool {
        self.boundary
    }
    #[must_use]
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

#[derive(Clone)]
struct PendingLine {
    final_line: usize,
    current_line: usize,
}

struct PendingOrigin {
    commit: ObjectId,
    path: Vec<u8>,
    lines: Vec<PendingLine>,
}

struct Attribution {
    commit: ObjectId,
    path: Vec<u8>,
    original_line: usize,
    boundary: bool,
}

struct ParentCandidate {
    commit: ObjectId,
    path: Vec<u8>,
    mapping: Vec<Option<usize>>,
}

impl Repository {
    /// Attribute every line of `path` at `revision` to the commit which last
    /// introduced it.
    ///
    /// Equal lines are passed through all parents (or only the first parent),
    /// and exact whole-file renames are followed when the path disappears.
    /// Line numbers are one-based and returned in final-file order.
    ///
    /// # Errors
    /// Returns an error for an invalid revision/path, non-blob target, corrupt
    /// history, or exceeded traversal, tree, line, object, or diff bounds.
    pub fn blame(
        &self,
        revision: &str,
        path: &[u8],
        options: &BlameOptions,
    ) -> Result<Vec<BlameLine>> {
        validate_blame_path(path)?;
        let expression = format!("{revision}^{{commit}}");
        let tip = self.resolve_revision(&expression, &options.revision)?.id;
        let mut tree_entries = 0;
        let tip_commit = self.read_commit(tip, options.revision.max_object_size)?;
        let tip_blob = blob_at_path(self, tip_commit.tree(), path, options, &mut tree_entries)?
            .ok_or_else(|| Error::InvalidRevision("blame path does not exist".into()))?;
        let final_data = read_blob(self, tip_blob, options.revision.max_object_size)?;
        let final_lines = split_owned_lines(&final_data, options.max_lines)?;
        let mut attributions = (0..final_lines.len()).map(|_| None).collect::<Vec<_>>();
        let mut queue = VecDeque::from([PendingOrigin {
            commit: tip,
            path: path.to_vec(),
            lines: (0..final_lines.len())
                .map(|line| PendingLine {
                    final_line: line,
                    current_line: line,
                })
                .collect(),
        }]);
        let mut visited = 0usize;

        while let Some(origin) = queue.pop_front() {
            visited = visited
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepository("blame commit count overflow".into()))?;
            if visited > options.max_commits {
                return Err(Error::InvalidRepository(format!(
                    "blame exceeds {} commits",
                    options.max_commits
                )));
            }
            let commit = self.read_commit(origin.commit, options.revision.max_object_size)?;
            let current_blob = blob_at_path(
                self,
                commit.tree(),
                &origin.path,
                options,
                &mut tree_entries,
            )?
            .ok_or_else(|| Error::InvalidRepository("blame origin path disappeared".into()))?;
            let current_data = read_blob(self, current_blob, options.revision.max_object_size)?;
            let parents = if options.first_parent {
                &commit.parents()[..commit.parents().len().min(1)]
            } else {
                commit.parents()
            };
            let candidates = blame_parent_candidates(
                self,
                parents,
                &origin.path,
                current_blob,
                &current_data,
                options,
                &mut tree_entries,
            )?;

            let boundary = parents.is_empty();
            let mut transferred = vec![Vec::new(); candidates.len()];
            for line in origin.lines {
                let parent = candidates
                    .iter()
                    .enumerate()
                    .find_map(|(index, candidate)| {
                        candidate
                            .mapping
                            .get(line.current_line)
                            .copied()
                            .flatten()
                            .map(|old| (index, old))
                    });
                if let Some((index, old_line)) = parent {
                    transferred[index].push(PendingLine {
                        final_line: line.final_line,
                        current_line: old_line,
                    });
                } else {
                    attributions[line.final_line] = Some(Attribution {
                        commit: origin.commit,
                        path: origin.path.clone(),
                        original_line: line.current_line,
                        boundary,
                    });
                }
            }
            for (candidate, lines) in candidates.into_iter().zip(transferred) {
                if !lines.is_empty() {
                    queue.push_back(PendingOrigin {
                        commit: candidate.commit,
                        path: candidate.path,
                        lines,
                    });
                }
            }
        }

        build_blame_lines(self, attributions, &final_lines, options)
    }
}

fn blame_parent_candidates(
    repository: &Repository,
    parents: &[ObjectId],
    current_path: &[u8],
    current_blob: ObjectId,
    current_data: &[u8],
    options: &BlameOptions,
    tree_entries: &mut usize,
) -> Result<Vec<ParentCandidate>> {
    let mut candidates = Vec::new();
    let current_line_count = split_owned_lines(current_data, options.max_lines)?.len();
    for parent_id in parents {
        let parent = repository.read_commit(*parent_id, options.revision.max_object_size)?;
        let mut path = current_path.to_vec();
        let mut blob = blob_at_path(repository, parent.tree(), &path, options, tree_entries)?;
        if blob.is_none()
            && let Some(found) = find_blob_path(
                repository,
                parent.tree(),
                current_blob,
                options,
                tree_entries,
            )?
        {
            path = found;
            blob = Some(current_blob);
        }
        let Some(blob) = blob else { continue };
        let mapping = if blob == current_blob {
            (0..current_line_count).map(Some).collect()
        } else {
            let parent_data = read_blob(repository, blob, options.revision.max_object_size)?;
            crate::diff::unchanged_line_map(
                &parent_data,
                current_data,
                options.max_lines,
                options.max_trace_cells,
            )?
        };
        candidates.push(ParentCandidate {
            commit: *parent_id,
            path,
            mapping,
        });
    }
    Ok(candidates)
}

fn build_blame_lines(
    repository: &Repository,
    attributions: Vec<Option<Attribution>>,
    final_lines: &[Vec<u8>],
    options: &BlameOptions,
) -> Result<Vec<BlameLine>> {
    let mut authors: BTreeMap<ObjectId, Signature> = BTreeMap::new();
    let mut result = Vec::with_capacity(attributions.len());
    for (index, attribution) in attributions.into_iter().enumerate() {
        let attribution = attribution
            .ok_or_else(|| Error::InvalidRepository("blame left a line unattributed".into()))?;
        let author = if let Some(author) = authors.get(&attribution.commit) {
            author.clone()
        } else {
            let author = repository
                .read_commit(attribution.commit, options.revision.max_object_size)?
                .author()
                .clone();
            authors.insert(attribution.commit, author.clone());
            author
        };
        result.push(BlameLine {
            commit: attribution.commit,
            author,
            original_path: attribution.path,
            original_line: attribution.original_line + 1,
            final_line: index + 1,
            boundary: attribution.boundary,
            contents: final_lines[index].clone(),
        });
    }
    Ok(result)
}

fn validate_blame_path(path: &[u8]) -> Result<()> {
    if path.is_empty()
        || path.starts_with(b"/")
        || path.ends_with(b"/")
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || matches!(part, b"." | b".."))
        || path.contains(&0)
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(path).into_owned().into(),
        ));
    }
    Ok(())
}

fn blob_at_path(
    repository: &Repository,
    root: ObjectId,
    path: &[u8],
    options: &BlameOptions,
    inspected: &mut usize,
) -> Result<Option<ObjectId>> {
    let mut tree_id = root;
    let mut components = path.split(|byte| *byte == b'/').peekable();
    while let Some(component) = components.next() {
        let tree = repository.read_tree(tree_id, options.revision.max_object_size)?;
        *inspected = inspected.saturating_add(tree.entries().len());
        if *inspected > options.max_tree_entries {
            return Err(Error::InvalidRepository(format!(
                "blame exceeds {} tree entries",
                options.max_tree_entries
            )));
        }
        let Some(entry) = tree
            .entries()
            .iter()
            .find(|entry| entry.name() == component)
        else {
            return Ok(None);
        };
        if components.peek().is_some() {
            if entry.mode() != EntryMode::Tree {
                return Ok(None);
            }
            tree_id = entry.id();
        } else if matches!(
            entry.mode(),
            EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link
        ) {
            return Ok(Some(entry.id()));
        } else {
            return Ok(None);
        }
    }
    Ok(None)
}

fn find_blob_path(
    repository: &Repository,
    tree_id: ObjectId,
    wanted: ObjectId,
    options: &BlameOptions,
    inspected: &mut usize,
) -> Result<Option<Vec<u8>>> {
    let mut stack = vec![(tree_id, Vec::<u8>::new())];
    while let Some((tree_id, prefix)) = stack.pop() {
        let tree = repository.read_tree(tree_id, options.revision.max_object_size)?;
        *inspected = inspected.saturating_add(tree.entries().len());
        if *inspected > options.max_tree_entries {
            return Err(Error::InvalidRepository(format!(
                "blame exceeds {} tree entries",
                options.max_tree_entries
            )));
        }
        for entry in tree.entries().iter().rev() {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.name());
            if entry.mode() == EntryMode::Tree {
                stack.push((entry.id(), path));
            } else if entry.id() == wanted {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn read_blob(repository: &Repository, id: ObjectId, max_size: usize) -> Result<Vec<u8>> {
    let object = repository.read_object(id, max_size)?;
    if object.kind() != ObjectKind::Blob {
        return Err(Error::InvalidObject(format!("object {id} is not a blob")));
    }
    Ok(object.data().to_vec())
}

fn split_owned_lines(data: &[u8], max_lines: usize) -> Result<Vec<Vec<u8>>> {
    let lines = data
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if lines.len() > max_lines {
        return Err(Error::InvalidRepository(format!(
            "blame exceeds {max_lines} lines"
        )));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::BlameOptions;
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature,
        Tree, TreeEntry,
    };

    #[test]
    fn attributes_linear_changes_through_an_exact_rename() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"file", b"one\ntwo\nthree\n", "Root", 1);
        let changed = commit(
            &repository,
            &[root],
            b"file",
            b"one\nTWO\nthree\n",
            "Editor",
            2,
        );
        let renamed = commit(
            &repository,
            &[changed],
            b"renamed",
            b"one\nTWO\nthree\n",
            "Renamer",
            3,
        );

        let lines = repository
            .blame(&renamed.to_string(), b"renamed", &BlameOptions::default())
            .unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].commit(), root);
        assert_eq!(lines[0].author().name(), "Root");
        assert_eq!(lines[0].original_path(), b"file");
        assert_eq!(lines[0].original_line(), 1);
        assert!(lines[0].is_boundary());
        assert_eq!(lines[1].commit(), changed);
        assert_eq!(lines[1].author().name(), "Editor");
        assert_eq!(lines[1].contents(), b"TWO\n");
        assert!(!lines[1].is_boundary());
        assert_eq!(lines[2].commit(), root);
        assert_eq!(lines[2].original_line(), 3);
        assert_eq!(lines[2].final_line(), 3);
    }

    #[test]
    fn distributes_merge_lines_across_both_parents_and_honors_first_parent() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let base = commit(&repository, &[], b"file", b"a\nb\n", "Base", 1);
        let left = commit(&repository, &[base], b"file", b"left\nb\n", "Left", 2);
        let right = commit(&repository, &[base], b"file", b"a\nright\n", "Right", 3);
        let merge = commit(
            &repository,
            &[left, right],
            b"file",
            b"left\nright\n",
            "Merge",
            4,
        );

        let lines = repository
            .blame(&merge.to_string(), b"file", &BlameOptions::default())
            .unwrap();
        assert_eq!(lines[0].commit(), left);
        assert_eq!(lines[1].commit(), right);

        let first_parent = repository
            .blame(
                &merge.to_string(),
                b"file",
                &BlameOptions {
                    first_parent: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(first_parent[0].commit(), left);
        assert_eq!(first_parent[1].commit(), merge);
    }

    #[test]
    fn enforces_commit_and_line_bounds() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"file", b"a\nb\n", "Root", 1);
        let tip = commit(&repository, &[root], b"file", b"a\nb\n", "Tip", 2);
        assert!(
            repository
                .blame(
                    &tip.to_string(),
                    b"file",
                    &BlameOptions {
                        max_commits: 1,
                        ..Default::default()
                    },
                )
                .is_err()
        );
        assert!(
            repository
                .blame(
                    &tip.to_string(),
                    b"file",
                    &BlameOptions {
                        max_lines: 1,
                        ..Default::default()
                    },
                )
                .is_err()
        );
    }

    fn commit(
        repository: &Repository,
        parents: &[crate::ObjectId],
        path: &[u8],
        contents: &[u8],
        author: &str,
        timestamp: i64,
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, path.to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new(author, "blame@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository
            .write_commit(&builder.message(format!("{author}\n").into_bytes()).build())
            .unwrap()
    }
}
