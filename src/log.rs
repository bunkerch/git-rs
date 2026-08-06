//! Structured, bounded full-history commit logs.

use crate::{
    Commit, DiffEntry, DiffOptions, Error, ObjectId, Repository, Result, RevisionWalkOptions,
};

/// Selection and rendering options for [`Repository::log`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogOptions {
    pub walk: RevisionWalkOptions,
    /// Literal file or directory prefixes. Empty selects every commit.
    pub paths: Vec<Vec<u8>>,
    pub show_patch: bool,
    /// Emit one diff against every parent of a merge, like `git log -m`.
    pub diff_merges: bool,
    pub max_patch_bytes: usize,
    pub diff: DiffOptions,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            walk: RevisionWalkOptions::default(),
            paths: Vec::new(),
            show_patch: false,
            diff_merges: false,
            max_patch_bytes: 1024 * 1024 * 1024,
            diff: DiffOptions::default(),
        }
    }
}

/// Changes from one parent (or the empty tree for a root) to a logged commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogParentDiff {
    parent: Option<ObjectId>,
    changes: Vec<DiffEntry>,
    patch: Vec<u8>,
}

impl LogParentDiff {
    #[must_use]
    pub const fn parent(&self) -> Option<ObjectId> {
        self.parent
    }
    #[must_use]
    pub fn changes(&self) -> &[DiffEntry] {
        &self.changes
    }
    #[must_use]
    pub fn patch(&self) -> &[u8] {
        &self.patch
    }
}

/// One commit selected for a full-history log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    id: ObjectId,
    commit: Commit,
    parent_diffs: Vec<LogParentDiff>,
}

impl LogEntry {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
    #[must_use]
    pub const fn commit(&self) -> &Commit {
        &self.commit
    }
    #[must_use]
    pub fn parent_diffs(&self) -> &[LogParentDiff] {
        &self.parent_diffs
    }
}

impl Repository {
    /// Walk full history and return structured commit records.
    ///
    /// With path selection, a commit is retained when it differs from at least
    /// one parent for a selected literal prefix. This corresponds to Git's
    /// `--full-history` path model and does not rewrite parentage.
    ///
    /// # Errors
    /// Returns an error for unsafe paths, revision-walk failures, corrupt
    /// trees/objects, diff limits, or aggregate patch output above the limit.
    pub fn log(
        &self,
        include: &[ObjectId],
        exclude: &[ObjectId],
        options: &LogOptions,
    ) -> Result<Vec<LogEntry>> {
        for path in &options.paths {
            validate_log_path(path)?;
        }
        let output_limit = options.walk.max_count.unwrap_or(usize::MAX);
        if output_limit == 0 {
            return Ok(Vec::new());
        }
        let mut walk = options.walk.clone();
        walk.max_count = None;
        let revisions = self.walk_revisions(include, exclude, &walk)?;
        let mut result = Vec::with_capacity(revisions.len());
        let mut patch_bytes = 0usize;
        for revision in revisions {
            let commit = revision.commit();
            let parents = if commit.parents().is_empty() {
                vec![None]
            } else {
                commit.parents().iter().copied().map(Some).collect()
            };
            let needs_diffs = !options.paths.is_empty() || options.show_patch;
            let mut all_diffs = Vec::new();
            let mut selected = options.paths.is_empty();
            if needs_diffs {
                for parent in &parents {
                    let old_tree = parent
                        .map(|id| self.read_commit(id, options.diff.max_object_size))
                        .transpose()?
                        .map(|commit| commit.tree());
                    let changes = self
                        .diff_trees(old_tree, Some(commit.tree()), &options.diff)?
                        .into_iter()
                        .filter(|change| log_change_selected(change, &options.paths))
                        .collect::<Vec<_>>();
                    selected |= !changes.is_empty();
                    all_diffs.push((*parent, changes));
                }
            }
            if !selected {
                continue;
            }

            let mut parent_diffs = Vec::new();
            if needs_diffs {
                for (parent, changes) in all_diffs {
                    let mut patch = Vec::new();
                    if options.show_patch && (commit.parents().len() <= 1 || options.diff_merges) {
                        for change in &changes {
                            let rendered = self.render_patch(change, &options.diff)?;
                            patch_bytes =
                                patch_bytes.checked_add(rendered.len()).ok_or_else(|| {
                                    Error::InvalidRepository("log patch size overflow".into())
                                })?;
                            if patch_bytes > options.max_patch_bytes {
                                return Err(Error::InvalidRepository(format!(
                                    "log patches exceed {} bytes",
                                    options.max_patch_bytes
                                )));
                            }
                            patch.extend_from_slice(&rendered);
                        }
                    }
                    parent_diffs.push(LogParentDiff {
                        parent,
                        changes,
                        patch,
                    });
                }
            }
            result.push(LogEntry {
                id: revision.id(),
                commit: commit.clone(),
                parent_diffs,
            });
            if result.len() == output_limit {
                break;
            }
        }
        Ok(result)
    }
}

fn log_change_selected(change: &DiffEntry, paths: &[Vec<u8>]) -> bool {
    paths.is_empty()
        || paths.iter().any(|path| {
            change
                .old_path()
                .is_some_and(|candidate| path_matches(candidate, path))
                || change
                    .new_path()
                    .is_some_and(|candidate| path_matches(candidate, path))
        })
}

fn path_matches(candidate: &[u8], selected: &[u8]) -> bool {
    candidate == selected
        || (candidate.starts_with(selected) && candidate.get(selected.len()) == Some(&b'/'))
}

fn validate_log_path(path: &[u8]) -> Result<()> {
    if path.is_empty()
        || path.starts_with(b"/")
        || path.ends_with(b"/")
        || path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|part| matches!(part, b"." | b".." | b""))
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(path).into_owned().into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::LogOptions;
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository,
        RevisionWalkOptions, Signature, Tree, TreeEntry,
    };

    #[test]
    fn filters_full_history_before_applying_output_limit_and_renders_patches() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"one\n", b"other\n", 1);
        let unrelated = commit(&repository, &[root], b"one\n", b"changed\n", 2);
        let selected = commit(&repository, &[unrelated], b"two\n", b"changed\n", 3);
        let options = LogOptions {
            paths: vec![b"file".to_vec()],
            show_patch: true,
            walk: RevisionWalkOptions {
                max_count: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let entries = repository.log(&[selected], &[], &options).unwrap();
        assert_eq!(
            entries.iter().map(super::LogEntry::id).collect::<Vec<_>>(),
            [selected, root]
        );
        assert_eq!(entries[0].parent_diffs().len(), 1);
        let patch = String::from_utf8_lossy(entries[0].parent_diffs()[0].patch());
        assert!(patch.contains("-one"));
        assert!(patch.contains("+two"));
        assert_eq!(entries[0].parent_diffs()[0].changes().len(), 1);

        assert!(
            repository
                .log(
                    &[selected],
                    &[],
                    &LogOptions {
                        max_patch_bytes: 1,
                        ..options
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn full_history_keeps_path_changing_merge_and_can_diff_each_parent() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let base = commit(&repository, &[], b"base\n", b"base\n", 1);
        let left = commit(&repository, &[base], b"left\n", b"base\n", 2);
        let right = commit(&repository, &[base], b"base\n", b"right\n", 3);
        let merge = commit(&repository, &[left, right], b"left\n", b"right\n", 4);
        let entries = repository
            .log(
                &[merge],
                &[],
                &LogOptions {
                    paths: vec![b"file".to_vec()],
                    show_patch: true,
                    diff_merges: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            entries.iter().map(super::LogEntry::id).collect::<Vec<_>>(),
            [merge, left, base]
        );
        assert_eq!(entries[0].parent_diffs().len(), 2);
        assert!(entries[0].parent_diffs()[0].changes().is_empty());
        assert_eq!(entries[0].parent_diffs()[1].changes().len(), 1);
    }

    fn commit(
        repository: &Repository,
        parents: &[crate::ObjectId],
        file: &[u8],
        other: &[u8],
        timestamp: i64,
    ) -> crate::ObjectId {
        let file = repository.write_object(ObjectKind::Blob, file).unwrap();
        let other = repository.write_object(ObjectKind::Blob, other).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), file).unwrap(),
                    TreeEntry::new(EntryMode::Blob, b"other".to_vec(), other).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("Log", "log@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository
            .write_commit(
                &builder
                    .message(format!("commit {timestamp}\n").into_bytes())
                    .build(),
            )
            .unwrap()
    }
}
