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

/// Resource bounds for reflog-aware fork-point discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkPointOptions {
    pub graph: GraphOptions,
    pub max_reflog_entries: usize,
}

impl Default for ForkPointOptions {
    fn default() -> Self {
        Self {
            graph: GraphOptions::default(),
            max_reflog_entries: 10_000_000,
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
        self.merge_bases_many(one, &[two], options)
    }

    /// Find all best common ancestors between `one` and a hypothetical merge
    /// commit whose parents are `others`.
    ///
    /// # Errors
    /// Returns an error when `others` is empty, for missing/corrupt commits, or
    /// when a graph resource limit is exceeded.
    pub fn merge_bases_many(
        &self,
        one: ObjectId,
        others: &[ObjectId],
        options: &GraphOptions,
    ) -> Result<Vec<ObjectId>> {
        if others.is_empty() {
            return Err(Error::InvalidRevision(
                "merge-base requires at least two commits".into(),
            ));
        }
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_commits,
            max_object_size: options.max_object_size,
        })?;
        let mut cache = HashMap::new();
        let left = self.load_reachable(&[one], false, options, &shallow, &mut cache)?;
        let right = self.load_reachable(others, false, options, &shallow, &mut cache)?;
        let common = left.intersection(&right).copied().collect::<HashSet<_>>();
        Ok(best_commits(&common, &cache))
    }

    /// Find all best common ancestors suitable for a single n-way merge.
    ///
    /// # Errors
    /// Returns an error when fewer than two commits are supplied, for
    /// missing/corrupt commits, or when a graph resource limit is exceeded.
    pub fn octopus_merge_bases(
        &self,
        commits: &[ObjectId],
        options: &GraphOptions,
    ) -> Result<Vec<ObjectId>> {
        if commits.len() < 2 {
            return Err(Error::InvalidRevision(
                "octopus merge-base requires at least two commits".into(),
            ));
        }
        let mut result = vec![commits[0]];
        for commit in &commits[1..] {
            let mut next = Vec::new();
            for base in &result {
                next.extend(self.merge_bases(*commit, *base, options)?);
            }
            result = next;
        }
        self.independent_commits(&result, options)
    }

    /// Remove commits reachable from another commit in the input.
    ///
    /// Duplicate IDs are collapsed and surviving commits retain first-input
    /// order.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt commits or an exceeded graph limit.
    pub fn independent_commits(
        &self,
        commits: &[ObjectId],
        options: &GraphOptions,
    ) -> Result<Vec<ObjectId>> {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for id in commits {
            if seen.insert(*id) {
                self.read_commit(*id, options.max_object_size)?;
                unique.push(*id);
            }
        }
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_commits,
            max_object_size: options.max_object_size,
        })?;
        let candidates = unique.iter().copied().collect::<HashSet<_>>();
        let mut redundant = HashSet::new();
        let mut cache = HashMap::new();
        for root in &unique {
            let reachable = self.load_reachable(&[*root], false, options, &shallow, &mut cache)?;
            for candidate in reachable.intersection(&candidates) {
                if candidate != root {
                    redundant.insert(*candidate);
                }
            }
        }
        Ok(unique
            .into_iter()
            .filter(|id| !redundant.contains(id))
            .collect())
    }

    /// Find where `derived` forked from the recorded history of `reference`.
    ///
    /// A result is returned only when the multi-input merge-base is unique and
    /// is itself a current or historical reflog value.
    ///
    /// # Errors
    /// Returns an error for a missing or ambiguous ref, malformed reflog data,
    /// a non-commit object, or an exceeded resource limit.
    pub fn fork_point(
        &self,
        reference: &str,
        derived: ObjectId,
        options: &ForkPointOptions,
    ) -> Result<Option<ObjectId>> {
        self.read_commit(derived, options.graph.max_object_size)?;
        let (full_name, current) = self.resolve_fork_point_reference(reference)?;
        let entries = self.read_reflog_bounded(&full_name, options.max_reflog_entries)?;
        let mut tips = Vec::new();
        if let Some(first) = entries.first()
            && !first.old_id().is_null()
        {
            tips.push(first.old_id());
        }
        tips.extend(
            entries
                .iter()
                .map(crate::ReflogEntry::new_id)
                .filter(|id| !id.is_null()),
        );
        if tips.is_empty() {
            tips.push(current);
        }
        let mut seen = HashSet::new();
        tips.retain(|id| seen.insert(*id));
        let bases = self.merge_bases_many(derived, &tips, &options.graph)?;
        if bases.len() == 1 && tips.contains(&bases[0]) {
            Ok(Some(bases[0]))
        } else {
            Ok(None)
        }
    }

    fn resolve_fork_point_reference(&self, name: &str) -> Result<(String, ObjectId)> {
        let candidates = if name == "HEAD" || name.starts_with("refs/") {
            vec![name.to_owned()]
        } else {
            vec![
                name.to_owned(),
                format!("refs/{name}"),
                format!("refs/tags/{name}"),
                format!("refs/heads/{name}"),
                format!("refs/remotes/{name}"),
                format!("refs/remotes/{name}/HEAD"),
            ]
        };
        let mut matches = Vec::new();
        for candidate in candidates {
            match self.resolve_reference(&candidate) {
                Ok(id) => matches.push((candidate, id)),
                Err(Error::NotFound(_) | Error::InvalidReferenceName(_)) => {}
                Err(error) => return Err(error),
            }
        }
        match matches.len() {
            0 => Err(Error::InvalidRevision(format!("no such reference: {name}"))),
            1 => Ok(matches.remove(0)),
            _ => Err(Error::InvalidRevision(format!(
                "ambiguous reference: {name}"
            ))),
        }
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

fn best_commits(common: &HashSet<ObjectId>, cache: &HashMap<ObjectId, Commit>) -> Vec<ObjectId> {
    let ordered = topo_order(common, cache, false);
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
    bases
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
    use super::{ForkPointOptions, GraphOptions, RevisionWalkOptions};
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, PreviousValue, ReferenceName, Repository,
        Signature, Tree,
    };

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
    fn distinguishes_hypothetical_and_octopus_bases_and_reduces_heads() {
        let repository = repository();
        let root = commit(&repository, &[], 1, b"root");
        let left = commit(&repository, &[root], 2, b"left");
        let left_tip = commit(&repository, &[left], 3, b"left-tip");
        let right = commit(&repository, &[root], 4, b"right");
        let options = GraphOptions::default();

        assert_eq!(
            repository
                .merge_bases_many(left, &[left_tip, right], &options)
                .unwrap(),
            [left]
        );
        assert_eq!(
            repository
                .octopus_merge_bases(&[left, left_tip, right], &options)
                .unwrap(),
            [root]
        );
        assert_eq!(
            repository
                .independent_commits(&[root, left, right, left], &options)
                .unwrap(),
            [left, right]
        );
        assert!(repository.merge_bases_many(left, &[], &options).is_err());
        assert!(repository.octopus_merge_bases(&[left], &options).is_err());
    }

    #[test]
    fn finds_fork_point_from_historical_reflog_tip_and_supports_dwim() {
        let repository = repository();
        let root = commit(&repository, &[], 1, b"root");
        let old_tip = commit(&repository, &[root], 2, b"old-tip");
        let new_tip = commit(&repository, &[old_tip], 3, b"new-tip");
        let derived = commit(&repository, &[old_tip], 4, b"derived");
        let identity = Signature::new("Graph", "graph@example.com", 5, 0).unwrap();
        let name = ReferenceName::branch("upstream").unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                old_tip,
                PreviousValue::MustNotExist,
                &identity,
                b"initial",
            )
            .unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                new_tip,
                PreviousValue::MustExist(old_tip),
                &identity,
                b"advance",
            )
            .unwrap();

        assert_eq!(
            repository
                .fork_point("upstream", derived, &ForkPointOptions::default())
                .unwrap(),
            Some(old_tip)
        );
        assert_eq!(
            repository
                .fork_point("refs/heads/upstream", new_tip, &ForkPointOptions::default())
                .unwrap(),
            Some(new_tip)
        );
        assert!(
            repository
                .fork_point(
                    "upstream",
                    derived,
                    &ForkPointOptions {
                        max_reflog_entries: 1,
                        ..ForkPointOptions::default()
                    }
                )
                .is_err()
        );
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
