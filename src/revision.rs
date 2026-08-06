//! Bounded commit-graph traversal and merge-base queries.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use crate::{Commit, Error, ObjectId, Repository, Result};

/// Resource limits shared by graph queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphOptions {
    pub max_commits: usize,
    pub max_object_size: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            max_commits: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// Revision-walk ordering and limiting choices.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevisionWalkOptions {
    pub graph: GraphOptions,
    pub max_count: Option<usize>,
    pub first_parent: bool,
}

/// One commit yielded in topological, newest-first tie-break order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    id: ObjectId,
    commit: Commit,
}

impl Revision {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub const fn commit(&self) -> &Commit {
        &self.commit
    }
}

impl Repository {
    /// Walk commits reachable from `include` but not from `exclude`.
    ///
    /// Descendants always precede parents. Among commits simultaneously ready,
    /// committer timestamp and then object ID provide deterministic ordering.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt commits or when `max_commits` is
    /// exceeded while discovering either side of the walk.
    pub fn walk_revisions(
        &self,
        include: &[ObjectId],
        exclude: &[ObjectId],
        options: &RevisionWalkOptions,
    ) -> Result<Vec<Revision>> {
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.graph.max_commits,
            max_object_size: options.graph.max_object_size,
        })?;
        let mut cache = HashMap::new();
        let included = self.load_reachable(
            include,
            options.first_parent,
            &options.graph,
            &shallow,
            &mut cache,
        )?;
        let excluded = self.load_reachable(
            exclude,
            options.first_parent,
            &options.graph,
            &shallow,
            &mut cache,
        )?;
        let visible = included
            .difference(&excluded)
            .copied()
            .collect::<HashSet<_>>();
        let ordered = topo_order(&visible, &cache, options.first_parent);
        let limit = options.max_count.unwrap_or(usize::MAX);
        Ok(ordered
            .into_iter()
            .take(limit)
            .map(|id| Revision {
                id,
                commit: cache[&id].clone(),
            })
            .collect())
    }

    /// Test whether `ancestor` is reachable from `descendant`, inclusively.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt commits or a graph resource-limit
    /// violation.
    pub fn is_ancestor(
        &self,
        ancestor: ObjectId,
        descendant: ObjectId,
        options: &GraphOptions,
    ) -> Result<bool> {
        self.read_commit(ancestor, options.max_object_size)?;
        if ancestor == descendant {
            return Ok(true);
        }
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_commits,
            max_object_size: options.max_object_size,
        })?;
        if shallow.is_empty() && !self.has_active_replacements()? {
            match self.read_commit_graph(options.max_object_size, options.max_commits) {
                Ok(graph) => {
                    if let Some(result) =
                        graph.is_ancestor(ancestor, descendant, options.max_commits)?
                    {
                        return Ok(result);
                    }
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let mut pending = vec![descendant];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            enforce_limit(seen.len(), options.max_commits)?;
            let commit = self.read_commit(id, options.max_object_size)?;
            if shallow.contains(&id) {
                continue;
            }
            if commit.parents().contains(&ancestor) {
                return Ok(true);
            }
            pending.extend(commit.parents().iter().rev().copied());
        }
        Ok(false)
    }

    /// Find all best common ancestors of two commits.
    ///
    /// A returned base is never an ancestor of another returned base, which is
    /// required for criss-cross histories where more than one base exists.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt commits or a graph resource-limit
    /// violation.
    pub fn merge_bases(
        &self,
        one: ObjectId,
        two: ObjectId,
        options: &GraphOptions,
    ) -> Result<Vec<ObjectId>> {
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_commits,
            max_object_size: options.max_object_size,
        })?;
        let mut cache = HashMap::new();
        let left = self.load_reachable(&[one], false, options, &shallow, &mut cache)?;
        let right = self.load_reachable(&[two], false, options, &shallow, &mut cache)?;
        let common = left.intersection(&right).copied().collect::<HashSet<_>>();
        let ordered = topo_order(&common, &cache, false);
        let mut stale = HashSet::new();
        let mut bases = Vec::new();
        for id in ordered {
            if stale.contains(&id) {
                continue;
            }
            bases.push(id);
            let mut pending = cache[&id].parents().to_vec();
            while let Some(parent) = pending.pop() {
                if common.contains(&parent) && stale.insert(parent) {
                    pending.extend(cache[&parent].parents());
                }
            }
        }
        bases.sort_unstable_by_key(|id| (Reverse(cache[id].committer().timestamp()), Reverse(*id)));
        Ok(bases)
    }

    fn load_reachable(
        &self,
        roots: &[ObjectId],
        first_parent: bool,
        options: &GraphOptions,
        shallow: &BTreeSet<ObjectId>,
        cache: &mut HashMap<ObjectId, Commit>,
    ) -> Result<HashSet<ObjectId>> {
        let mut seen = HashSet::new();
        let mut pending = roots.iter().rev().copied().collect::<Vec<_>>();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            enforce_limit(seen.len(), options.max_commits)?;
            if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(id) {
                entry.insert(self.read_commit(id, options.max_object_size)?);
            }
            if shallow.contains(&id) {
                continue;
            }
            let parents = cache[&id].parents();
            if first_parent {
                pending.extend(parents.first());
            } else {
                pending.extend(parents.iter().rev().copied());
            }
        }
        Ok(seen)
    }
}

fn topo_order(
    visible: &HashSet<ObjectId>,
    commits: &HashMap<ObjectId, Commit>,
    first_parent: bool,
) -> Vec<ObjectId> {
    let mut child_counts = visible
        .iter()
        .map(|id| (*id, 0usize))
        .collect::<HashMap<_, _>>();
    for id in visible {
        for parent in selected_parents(&commits[id], first_parent) {
            if let Some(count) = child_counts.get_mut(parent) {
                *count += 1;
            }
        }
    }
    let mut ready = BinaryHeap::new();
    for (id, count) in &child_counts {
        if *count == 0 {
            ready.push((commits[id].committer().timestamp(), *id));
        }
    }
    let mut output = Vec::with_capacity(visible.len());
    while let Some((_, id)) = ready.pop() {
        output.push(id);
        for parent in selected_parents(&commits[&id], first_parent) {
            let Some(count) = child_counts.get_mut(parent) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                ready.push((commits[parent].committer().timestamp(), *parent));
            }
        }
    }
    debug_assert_eq!(output.len(), visible.len());
    output
}

fn selected_parents(commit: &Commit, first_parent: bool) -> &[ObjectId] {
    if first_parent {
        &commit.parents()[..commit.parents().len().min(1)]
    } else {
        commit.parents()
    }
}

fn enforce_limit(count: usize, limit: usize) -> Result<()> {
    if count > limit {
        return Err(Error::InvalidRepository(format!(
            "revision walk exceeds {limit} commits"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GraphOptions, RevisionWalkOptions};
    use crate::{CommitBuilder, InitOptions, MemoryFileSystem, Repository, Signature, Tree};

    #[test]
    fn walks_topologically_hides_ancestors_and_supports_first_parent() {
        let repository = repository();
        let root = commit(&repository, &[], 1, b"root");
        let left = commit(&repository, &[root], 2, b"left");
        let right = commit(&repository, &[root], 3, b"right");
        let merge = commit(&repository, &[left, right], 4, b"merge");

        let walked = repository
            .walk_revisions(&[merge], &[root], &RevisionWalkOptions::default())
            .unwrap()
            .into_iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>();
        assert_eq!(walked, [merge, right, left]);
        assert!(position(&walked, merge) < position(&walked, left));
        assert!(position(&walked, merge) < position(&walked, right));

        let first_parent = repository
            .walk_revisions(
                &[merge],
                &[],
                &RevisionWalkOptions {
                    first_parent: true,
                    ..RevisionWalkOptions::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>();
        assert_eq!(first_parent, [merge, left, root]);
    }

    #[test]
    fn finds_best_bases_for_linear_and_criss_cross_histories() {
        let repository = repository();
        let root = commit(&repository, &[], 1, b"root");
        let left = commit(&repository, &[root], 2, b"left");
        let right = commit(&repository, &[root], 3, b"right");
        let left_tip = commit(&repository, &[left, right], 4, b"left-tip");
        let right_tip = commit(&repository, &[right, left], 5, b"right-tip");
        let options = GraphOptions::default();

        assert!(repository.is_ancestor(root, left_tip, &options).unwrap());
        assert!(!repository.is_ancestor(left_tip, root, &options).unwrap());
        assert_eq!(
            repository.merge_bases(root, left_tip, &options).unwrap(),
            [root]
        );

        let mut bases = repository
            .merge_bases(left_tip, right_tip, &options)
            .unwrap();
        bases.sort_unstable();
        let mut expected = vec![left, right];
        expected.sort_unstable();
        assert_eq!(bases, expected);
    }

    #[test]
    fn enforces_discovery_limits_before_unbounded_walks() {
        let repository = repository();
        let root = commit(&repository, &[], 1, b"root");
        let tip = commit(&repository, &[root], 2, b"tip");
        assert!(
            repository
                .walk_revisions(
                    &[tip],
                    &[],
                    &RevisionWalkOptions {
                        graph: GraphOptions {
                            max_commits: 1,
                            ..GraphOptions::default()
                        },
                        ..RevisionWalkOptions::default()
                    },
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
        message: &[u8],
    ) -> crate::ObjectId {
        let tree = repository
            .write_tree(&Tree::new(Vec::new()).unwrap())
            .unwrap();
        let identity = Signature::new("Graph", "graph@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository
            .write_commit(&builder.message(message.to_vec()).build())
            .unwrap()
    }

    fn position(ids: &[crate::ObjectId], id: crate::ObjectId) -> usize {
        ids.iter().position(|candidate| *candidate == id).unwrap()
    }
}
