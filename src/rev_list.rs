//! Structured commit and object reachability enumeration.

use std::collections::HashSet;

use crate::{
    Commit, EntryMode, Error, GraphOptions, ObjectId, ObjectKind, Repository, Result,
    RevisionWalkOptions,
};

/// Selection, ordering, and object-enumeration options for rev-list queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevListOptions {
    pub graph: GraphOptions,
    pub max_count: Option<usize>,
    pub parents: RevListParents,
    pub order: RevListOrder,
    pub boundary: bool,
    pub objects: bool,
    pub max_objects: usize,
}

/// Which parent edges participate in reachability.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RevListParents {
    #[default]
    All,
    First,
}

/// Direction of the final topological output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RevListOrder {
    #[default]
    Topological,
    Reverse,
}

impl Default for RevListOptions {
    fn default() -> Self {
        Self {
            graph: GraphOptions::default(),
            max_count: None,
            parents: RevListParents::All,
            order: RevListOrder::Topological,
            boundary: false,
            objects: false,
            max_objects: 10_000_000,
        }
    }
}

/// Reachability side assigned to a listed commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevListSide {
    Unmarked,
    Left,
    Right,
    Boundary,
}

/// One selected commit and its complete parsed metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevListCommit {
    id: ObjectId,
    commit: Commit,
    side: RevListSide,
}

impl RevListCommit {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub const fn commit(&self) -> &Commit {
        &self.commit
    }

    #[must_use]
    pub const fn side(&self) -> RevListSide {
        self.side
    }
}

/// A non-commit object reached through a selected commit tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevListObject {
    id: ObjectId,
    kind: ObjectKind,
    path: Vec<u8>,
}

impl RevListObject {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// Return the first tree path through which the object was discovered.
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
}

/// Structured output from a rev-list query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevListResult {
    commits: Vec<RevListCommit>,
    objects: Vec<RevListObject>,
}

impl RevListResult {
    #[must_use]
    pub fn commits(&self) -> &[RevListCommit] {
        &self.commits
    }

    #[must_use]
    pub fn objects(&self) -> &[RevListObject] {
        &self.objects
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.commits.len()
    }

    #[must_use]
    pub fn left_count(&self) -> usize {
        self.commits
            .iter()
            .filter(|entry| entry.side == RevListSide::Left)
            .count()
    }

    #[must_use]
    pub fn right_count(&self) -> usize {
        self.commits
            .iter()
            .filter(|entry| entry.side == RevListSide::Right)
            .count()
    }
}

impl Repository {
    /// List commits reachable from `include` but not `exclude` in topological,
    /// newest-first order, with optional boundaries and tree objects.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt objects or exceeded graph/object
    /// limits.
    pub fn rev_list(
        &self,
        include: &[ObjectId],
        exclude: &[ObjectId],
        options: &RevListOptions,
    ) -> Result<RevListResult> {
        let walk = walk_options(options);
        let revisions = self.walk_revisions(include, exclude, &walk)?;
        let excluded = if options.boundary && !exclude.is_empty() {
            self.walk_revisions(
                exclude,
                &[],
                &RevisionWalkOptions {
                    graph: options.graph.clone(),
                    first_parent: options.parents == RevListParents::First,
                    ..RevisionWalkOptions::default()
                },
            )?
            .into_iter()
            .map(|entry| entry.id())
            .collect()
        } else {
            HashSet::new()
        };
        self.finish_rev_list(revisions, &excluded, None, options)
    }

    /// List the symmetric difference `left...right`, marking which side alone
    /// reaches each commit.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt commits or exceeded resource limits.
    pub fn rev_list_symmetric(
        &self,
        left: ObjectId,
        right: ObjectId,
        options: &RevListOptions,
    ) -> Result<RevListResult> {
        let bases = self.merge_bases(left, right, &options.graph)?;
        let unbounded = RevisionWalkOptions {
            graph: options.graph.clone(),
            first_parent: options.parents == RevListParents::First,
            ..RevisionWalkOptions::default()
        };
        let left_ids = self
            .walk_revisions(&[left], &bases, &unbounded)?
            .into_iter()
            .map(|entry| entry.id())
            .collect::<HashSet<_>>();
        let right_ids = self
            .walk_revisions(&[right], &bases, &unbounded)?
            .into_iter()
            .map(|entry| entry.id())
            .collect::<HashSet<_>>();
        let mut revisions = self.walk_revisions(&[left, right], &bases, &walk_options(options))?;
        order_symmetric_sides(&mut revisions, &left_ids, &right_ids);
        let excluded = if options.boundary {
            self.walk_revisions(&bases, &[], &unbounded)?
                .into_iter()
                .map(|entry| entry.id())
                .collect()
        } else {
            HashSet::new()
        };
        self.finish_rev_list(revisions, &excluded, Some((&left_ids, &right_ids)), options)
    }

    fn finish_rev_list(
        &self,
        revisions: Vec<crate::Revision>,
        excluded: &HashSet<ObjectId>,
        sides: Option<(&HashSet<ObjectId>, &HashSet<ObjectId>)>,
        options: &RevListOptions,
    ) -> Result<RevListResult> {
        let visible = revisions
            .iter()
            .map(crate::Revision::id)
            .collect::<HashSet<_>>();
        let mut commits = revisions
            .into_iter()
            .map(|revision| {
                let side = sides.map_or(RevListSide::Unmarked, |(left, right)| {
                    if left.contains(&revision.id()) {
                        RevListSide::Left
                    } else if right.contains(&revision.id()) {
                        RevListSide::Right
                    } else {
                        RevListSide::Unmarked
                    }
                });
                RevListCommit {
                    id: revision.id(),
                    commit: revision.commit().clone(),
                    side,
                }
            })
            .collect::<Vec<_>>();
        if options.boundary {
            let mut boundary = Vec::new();
            let mut seen_boundary = HashSet::new();
            for entry in &commits {
                for parent in
                    selected_parents(entry.commit(), options.parents == RevListParents::First)
                {
                    if !visible.contains(parent)
                        && excluded.contains(parent)
                        && seen_boundary.insert(*parent)
                    {
                        boundary.push(*parent);
                    }
                }
            }
            for id in boundary {
                commits.push(RevListCommit {
                    id,
                    commit: self.read_commit(id, options.graph.max_object_size)?,
                    side: RevListSide::Boundary,
                });
            }
        }
        if options.order == RevListOrder::Reverse {
            commits.reverse();
        }
        let objects = if options.objects {
            self.rev_list_objects(&commits, options)?
        } else {
            Vec::new()
        };
        Ok(RevListResult { commits, objects })
    }

    fn rev_list_objects(
        &self,
        commits: &[RevListCommit],
        options: &RevListOptions,
    ) -> Result<Vec<RevListObject>> {
        let mut output = Vec::new();
        let mut seen = HashSet::new();
        let mut pending = commits
            .iter()
            .filter(|entry| entry.side != RevListSide::Boundary)
            .rev()
            .map(|entry| (entry.commit.tree(), Vec::new()))
            .collect::<Vec<_>>();
        while let Some((id, path)) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            enforce_object_limit(seen.len(), options.max_objects)?;
            let tree = self.read_tree(id, options.graph.max_object_size)?;
            output.push(RevListObject {
                id,
                kind: ObjectKind::Tree,
                path: path.clone(),
            });
            for entry in tree.entries().iter().rev() {
                let entry_path = join_path(&path, entry.name())?;
                match entry.mode() {
                    EntryMode::Tree => pending.push((entry.id(), entry_path)),
                    EntryMode::Gitlink => {}
                    EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link => {
                        if seen.insert(entry.id()) {
                            enforce_object_limit(seen.len(), options.max_objects)?;
                            self.read_object(entry.id(), options.graph.max_object_size)?;
                            output.push(RevListObject {
                                id: entry.id(),
                                kind: ObjectKind::Blob,
                                path: entry_path,
                            });
                        }
                    }
                }
            }
        }
        Ok(output)
    }
}

fn walk_options(options: &RevListOptions) -> RevisionWalkOptions {
    RevisionWalkOptions {
        graph: options.graph.clone(),
        max_count: options.max_count,
        first_parent: options.parents == RevListParents::First,
    }
}

fn order_symmetric_sides(
    revisions: &mut Vec<crate::Revision>,
    left: &HashSet<ObjectId>,
    right: &HashSet<ObjectId>,
) {
    let left_date = revisions
        .iter()
        .filter(|entry| left.contains(&entry.id()))
        .map(|entry| entry.commit().committer().timestamp())
        .max();
    let right_date = revisions
        .iter()
        .filter(|entry| right.contains(&entry.id()))
        .map(|entry| entry.commit().committer().timestamp())
        .max();
    let right_first = right_date > left_date;
    let mut first = Vec::with_capacity(revisions.len());
    let mut second = Vec::with_capacity(revisions.len());
    for entry in revisions.drain(..) {
        let belongs_right = right.contains(&entry.id());
        if belongs_right == right_first {
            first.push(entry);
        } else {
            second.push(entry);
        }
    }
    first.extend(second);
    *revisions = first;
}

fn selected_parents(commit: &Commit, first_parent: bool) -> &[ObjectId] {
    if first_parent {
        &commit.parents()[..commit.parents().len().min(1)]
    } else {
        commit.parents()
    }
}

fn join_path(parent: &[u8], name: &[u8]) -> Result<Vec<u8>> {
    let capacity = parent
        .len()
        .checked_add(usize::from(!parent.is_empty()))
        .and_then(|value| value.checked_add(name.len()))
        .ok_or_else(|| Error::InvalidRepository("rev-list object path length overflow".into()))?;
    let mut path = Vec::with_capacity(capacity);
    path.extend_from_slice(parent);
    if !parent.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    Ok(path)
}

fn enforce_object_limit(count: usize, limit: usize) -> Result<()> {
    if count > limit {
        return Err(Error::InvalidRepository(format!(
            "rev-list object traversal exceeds {limit} objects"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{RevListOptions, RevListOrder, RevListSide};
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature,
        Tree, TreeEntry,
    };

    #[test]
    fn lists_symmetric_sides_boundaries_reverse_and_counts() {
        let repository = repository();
        let base = commit(&repository, &[], 1, b"base");
        let left = commit(&repository, &[base], 2, b"left");
        let left_tip = commit(&repository, &[left], 3, b"left-tip");
        let right = commit(&repository, &[base], 3, b"right");

        let listed = repository
            .rev_list_symmetric(
                left_tip,
                right,
                &RevListOptions {
                    boundary: true,
                    ..RevListOptions::default()
                },
            )
            .unwrap();
        assert_eq!(listed.left_count(), 2);
        assert_eq!(listed.right_count(), 1);
        assert_eq!(listed.count(), 4);
        assert_eq!(listed.commits().first().unwrap().id(), left_tip);
        assert_eq!(listed.commits().last().unwrap().id(), base);
        assert_eq!(
            listed.commits().last().unwrap().side(),
            RevListSide::Boundary
        );
        for entry in &listed.commits()[..3] {
            assert_eq!(
                entry.side(),
                if entry.id() == right {
                    RevListSide::Right
                } else {
                    RevListSide::Left
                }
            );
        }

        let reversed = repository
            .rev_list_symmetric(
                left_tip,
                right,
                &RevListOptions {
                    boundary: true,
                    order: RevListOrder::Reverse,
                    ..RevListOptions::default()
                },
            )
            .unwrap();
        assert_eq!(reversed.commits().first().unwrap().id(), base);
        assert_eq!(reversed.commits().last().unwrap().id(), left_tip);
    }

    #[test]
    fn lists_normal_ranges_and_deduplicated_tree_object_closure() {
        let repository = repository();
        let base = commit(&repository, &[], 1, b"base");
        let tip = commit(&repository, &[base], 2, b"tip");
        let listed = repository
            .rev_list(
                &[tip],
                &[base],
                &RevListOptions {
                    objects: true,
                    ..RevListOptions::default()
                },
            )
            .unwrap();
        assert_eq!(listed.count(), 1);
        assert_eq!(listed.commits()[0].id(), tip);
        assert_eq!(listed.commits()[0].side(), RevListSide::Unmarked);
        assert!(
            listed
                .objects()
                .iter()
                .any(|object| object.kind() == ObjectKind::Tree && object.path().is_empty())
        );
        assert!(
            listed.objects().iter().any(|object| {
                object.kind() == ObjectKind::Tree && object.path() == b"directory"
            })
        );
        assert!(listed.objects().iter().any(|object| {
            object.kind() == ObjectKind::Blob && object.path() == b"directory/file"
        }));
        let ids = listed
            .objects()
            .iter()
            .map(super::RevListObject::id)
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), listed.objects().len());

        assert!(
            repository
                .rev_list(
                    &[tip],
                    &[],
                    &RevListOptions {
                        objects: true,
                        max_objects: 0,
                        ..RevListOptions::default()
                    }
                )
                .is_err()
        );
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }

    fn commit(
        repository: &Repository,
        parents: &[crate::ObjectId],
        timestamp: i64,
        contents: &[u8],
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let nested = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"directory".to_vec(), nested).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("List", "list@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository
            .write_commit(&builder.message(contents.to_vec()).build())
            .unwrap()
    }
}
