//! Patch-equivalence classification for commits unique to one side of a range.

use std::collections::HashSet;

use crate::{DiffOptions, GraphOptions, ObjectId, Repository, Result, RevisionWalkOptions};

/// Resource bounds used while classifying commits by patch identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CherryOptions {
    pub graph: GraphOptions,
    pub diff: DiffOptions,
}

/// One non-merge commit reachable only from the selected head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CherryCommit {
    id: ObjectId,
    equivalent_upstream: bool,
}

impl CherryCommit {
    #[must_use]
    pub const fn id(self) -> ObjectId {
        self.id
    }

    /// Whether upstream contains a commit with the same stable patch identity.
    #[must_use]
    pub const fn equivalent_upstream(self) -> bool {
        self.equivalent_upstream
    }

    /// The marker printed by `git cherry`: `-` for equivalent, `+` for unique.
    #[must_use]
    pub const fn marker(self) -> char {
        if self.equivalent_upstream { '-' } else { '+' }
    }
}

impl Repository {
    /// Classify non-merge commits in `head` but not `upstream` by patch identity.
    ///
    /// Results are oldest first. If supplied, `limit` and its ancestors are
    /// excluded from the head side, matching the third revision accepted by
    /// `git cherry`.
    ///
    /// # Errors
    /// Returns an error for malformed or missing commits, trees, or blobs, or
    /// when graph or diff resource limits are exceeded.
    pub fn cherry(
        &self,
        upstream: ObjectId,
        head: ObjectId,
        limit: Option<ObjectId>,
        options: &CherryOptions,
    ) -> Result<Vec<CherryCommit>> {
        if upstream == head {
            return Ok(Vec::new());
        }
        let walk_options = RevisionWalkOptions {
            graph: options.graph.clone(),
            max_count: None,
            first_parent: false,
        };
        let upstream_ids = self
            .walk_revisions(&[upstream], &[head], &walk_options)?
            .into_iter()
            .filter(|revision| revision.commit().parents().len() <= 1)
            .filter_map(|revision| {
                self.commit_patch_id(revision.id(), &options.diff)
                    .transpose()
            })
            .collect::<Result<HashSet<_>>>()?;

        let mut excluded = vec![upstream];
        excluded.extend(limit);
        let mut head_commits = self.walk_revisions(&[head], &excluded, &walk_options)?;
        head_commits.reverse();
        head_commits
            .into_iter()
            .filter(|revision| revision.commit().parents().len() <= 1)
            .map(|revision| {
                let Some(patch_id) = self.commit_patch_id(revision.id(), &options.diff)? else {
                    return Err(crate::Error::InvalidRepository(format!(
                        "commit {} changed from a non-merge during cherry classification",
                        revision.id()
                    )));
                };
                Ok(CherryCommit {
                    id: revision.id(),
                    equivalent_upstream: upstream_ids.contains(&patch_id),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::CherryOptions;
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature,
        Tree, TreeEntry,
    };

    #[test]
    fn classifies_equivalent_and_unique_commits_oldest_first() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let base = commit(&repository, None, b"base\n", 1);
        let upstream_equivalent = commit(&repository, Some(base), b"shared change\n", 2);
        let upstream = commit(
            &repository,
            Some(upstream_equivalent),
            b"upstream only\n",
            3,
        );
        let head_equivalent = commit(&repository, Some(base), b"shared   change\n", 4);
        let head = commit(&repository, Some(head_equivalent), b"head only\n", 5);

        let result = repository
            .cherry(upstream, head, None, &CherryOptions::default())
            .unwrap();
        assert_eq!(
            result
                .iter()
                .map(|commit| (commit.id(), commit.marker()))
                .collect::<Vec<_>>(),
            [(head_equivalent, '-'), (head, '+')]
        );
        assert_eq!(
            repository
                .cherry(
                    upstream,
                    head,
                    Some(head_equivalent),
                    &CherryOptions::default()
                )
                .unwrap()
                .iter()
                .map(|commit| commit.id())
                .collect::<Vec<_>>(),
            [head]
        );
    }

    fn commit(
        repository: &Repository,
        parent: Option<crate::ObjectId>,
        data: &[u8],
        timestamp: i64,
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, data).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("A", "a@example.com", timestamp, 0).unwrap();
        let mut builder =
            CommitBuilder::new(tree, identity.clone(), identity).message(b"message\n".to_vec());
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
