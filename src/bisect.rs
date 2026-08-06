//! Stateful, Git-compatible revision bisection.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::{
    Error, GraphOptions, ObjectId, PreviousValue, ReferenceEdit, ReferenceName, ReferenceTarget,
    Repository, Result, RevisionWalkOptions, Signature, SwitchOptions,
};

const BAD_REF: &str = "refs/bisect/bad";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisectOptions {
    pub no_checkout: bool,
    pub max_merge_ancestor_visits: usize,
    pub graph: GraphOptions,
}

impl Default for BisectOptions {
    fn default() -> Self {
        Self {
            no_checkout: false,
            max_merge_ancestor_visits: 100_000_000,
            graph: GraphOptions::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BisectMark {
    Good,
    Bad,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BisectOutcome {
    Test {
        commit: ObjectId,
        remaining: usize,
        estimated_steps: u32,
    },
    Found {
        commit: ObjectId,
    },
    OnlySkipped {
        commits: Vec<ObjectId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisectState {
    pub bad: ObjectId,
    pub good: Vec<ObjectId>,
    pub skipped: Vec<ObjectId>,
    pub start: String,
    pub current: Option<ObjectId>,
}

impl Repository {
    /// Start a default `bad`/`good` bisection and select its first test commit.
    ///
    /// State uses Git's standard files and `refs/bisect/` namespace. Every good
    /// commit must be an ancestor of `bad`.
    ///
    /// # Errors
    /// Returns an error for an existing session, missing/non-commit revisions,
    /// invalid ancestry, graph/resource limits, ref conflicts, checkout
    /// conflicts, or storage failures.
    pub fn start_bisect(
        &self,
        bad: ObjectId,
        good: &[ObjectId],
        options: &BisectOptions,
        committer: &Signature,
    ) -> Result<BisectOutcome> {
        if self.filesystem().exists(&self.git_path("BISECT_START"))? {
            return Err(Error::InvalidRepository(
                "a bisect session is already active".into(),
            ));
        }
        self.read_commit(bad, options.graph.max_object_size)?;
        if good.is_empty() {
            return Err(Error::InvalidRepository(
                "bisect requires at least one good commit".into(),
            ));
        }
        let mut unique_good = BTreeSet::new();
        for id in good {
            self.read_commit(*id, options.graph.max_object_size)?;
            if !unique_good.insert(*id) {
                return Err(Error::InvalidRepository(
                    "duplicate good bisect commit".into(),
                ));
            }
            if !self.is_ancestor(*id, bad, &options.graph)? {
                return Err(Error::InvalidRepository(format!(
                    "good commit {id} is not an ancestor of bad commit {bad}"
                )));
            }
        }
        let start = self.bisect_start_label()?;
        let mut edits = vec![ReferenceEdit::update(
            ReferenceName::new(BAD_REF)?,
            bad,
            PreviousValue::MustNotExist,
        )];
        for id in &unique_good {
            edits.push(ReferenceEdit::update(
                ReferenceName::new(format!("refs/bisect/good-{id}"))?,
                *id,
                PreviousValue::MustNotExist,
            ));
        }
        self.apply_reference_transaction(&edits)?;
        let state_result = (|| {
            self.write_atomic(Path::new("BISECT_START"), format!("{start}\n").as_bytes())?;
            self.write_atomic(Path::new("BISECT_TERMS"), b"bad\ngood\n")?;
            self.write_atomic(Path::new("BISECT_NAMES"), b"\n")?;
            let mut log = format!("# bad: [{bad}]\n");
            for id in &unique_good {
                writeln!(log, "# good: [{id}]")
                    .map_err(|_| Error::InvalidRepository("cannot format bisect log".into()))?;
            }
            self.write_atomic(Path::new("BISECT_LOG"), log.as_bytes())?;
            self.bisect_next(options, committer)
        })();
        if state_result.is_err() {
            let _ = self.clean_bisect_state();
        }
        state_result
    }

    /// Load the active bisection state from Git-compatible refs and files.
    ///
    /// # Errors
    /// Returns an error when no session is active or state is missing,
    /// malformed, symbolic where direct IDs are required, or unreadable.
    pub fn bisect_state(&self) -> Result<BisectState> {
        let start = self.read_bisect_text("BISECT_START")?;
        if start.is_empty() {
            return Err(Error::InvalidRepository(
                "no bisect session is active".into(),
            ));
        }
        if self.read_git_file("BISECT_TERMS")? != b"bad\ngood\n" {
            return Err(Error::InvalidRepository(
                "custom bisect terms are unsupported by this API".into(),
            ));
        }
        let bad = match self.read_reference(BAD_REF)?.target() {
            ReferenceTarget::Direct(id) => *id,
            ReferenceTarget::Symbolic(_) => {
                return Err(Error::InvalidRepository(
                    "bisect bad ref is symbolic".into(),
                ));
            }
        };
        let mut good = Vec::new();
        let mut skipped = Vec::new();
        for reference in self.references()? {
            let selected = reference.name().starts_with("refs/bisect/good-")
                || reference.name().starts_with("refs/bisect/skip-");
            if !selected {
                continue;
            }
            let ReferenceTarget::Direct(id) = reference.target() else {
                return Err(Error::InvalidRepository(format!(
                    "bisect state ref {} is symbolic",
                    reference.name()
                )));
            };
            if reference.name().starts_with("refs/bisect/good-") {
                good.push(*id);
            } else {
                skipped.push(*id);
            }
        }
        good.sort_unstable();
        good.dedup();
        skipped.sort_unstable();
        skipped.dedup();
        let current = match self.read_git_file("BISECT_HEAD") {
            Ok(contents) => Some(parse_pseudoref("BISECT_HEAD", &contents)?),
            Err(Error::NotFound(_)) => self.resolve_reference("HEAD").ok(),
            Err(error) => return Err(error),
        };
        Ok(BisectState {
            bad,
            good,
            skipped,
            start,
            current,
        })
    }

    /// Mark a commit and select the next candidate.
    ///
    /// When `commit` is `None`, the current `BISECT_HEAD` (no-checkout mode) or
    /// `HEAD` is marked. `bad` replaces the sole bad ref with exact CAS;
    /// `good` and `skip` create content-addressed refs idempotently.
    ///
    /// # Errors
    /// Returns an error for inactive/malformed state, a stale bad ref, graph or
    /// checkout failure, invalid mark ancestry, or storage errors.
    pub fn mark_bisect(
        &self,
        mark: BisectMark,
        commit: Option<ObjectId>,
        options: &BisectOptions,
        committer: &Signature,
    ) -> Result<BisectOutcome> {
        let state = self.bisect_state()?;
        let id = commit
            .or(state.current)
            .ok_or_else(|| Error::InvalidRepository("bisect has no current commit".into()))?;
        if commit.is_none() {
            let expected = parse_pseudoref(
                "BISECT_EXPECTED_REV",
                &self.read_git_file("BISECT_EXPECTED_REV")?,
            )?;
            if id != expected {
                return Err(Error::InvalidRepository(format!(
                    "expected to test {expected}, but current commit is {id}"
                )));
            }
        }
        self.read_commit(id, options.graph.max_object_size)?;
        match mark {
            BisectMark::Bad => {
                for good in &state.good {
                    if !self.is_ancestor(*good, id, &options.graph)? {
                        return Err(Error::InvalidRepository(format!(
                            "new bad commit {id} does not descend from good commit {good}"
                        )));
                    }
                }
                self.update_reference(
                    &ReferenceName::new(BAD_REF)?,
                    id,
                    PreviousValue::MustExist(state.bad),
                )?;
            }
            BisectMark::Good | BisectMark::Skip => {
                if !self.is_ancestor(id, state.bad, &options.graph)? {
                    return Err(Error::InvalidRepository(format!(
                        "marked commit {id} is not an ancestor of bad commit {}",
                        state.bad
                    )));
                }
                let term = if mark == BisectMark::Good {
                    "good"
                } else {
                    "skip"
                };
                let name = ReferenceName::new(format!("refs/bisect/{term}-{id}"))?;
                match self.update_reference(&name, id, PreviousValue::MustNotExist) {
                    Ok(()) => {}
                    Err(Error::ReferenceConflict(_))
                        if self.resolve_reference(name.as_str())? == id => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.append_bisect_log(mark, id)?;
        self.bisect_next(options, committer)
    }

    /// Recompute and select the next test commit from stored state.
    ///
    /// # Errors
    /// Returns an error for invalid state/ancestry, bounded graph calculation,
    /// checkout conflicts, or state publication failures.
    pub fn bisect_next(
        &self,
        options: &BisectOptions,
        committer: &Signature,
    ) -> Result<BisectOutcome> {
        let state = self.bisect_state()?;
        for good in &state.good {
            if !self.is_ancestor(*good, state.bad, &options.graph)? {
                return Err(Error::InvalidRepository(format!(
                    "good commit {good} is not an ancestor of bad commit {}",
                    state.bad
                )));
            }
        }
        let revisions = self.walk_revisions(
            &[state.bad],
            &state.good,
            &RevisionWalkOptions {
                graph: options.graph.clone(),
                ..RevisionWalkOptions::default()
            },
        )?;
        if revisions.is_empty() {
            return Err(Error::InvalidRepository(
                "bisect has no candidate commits".into(),
            ));
        }
        let candidates = revisions
            .iter()
            .map(crate::Revision::id)
            .collect::<HashSet<_>>();
        let skipped = state.skipped.iter().copied().collect::<HashSet<_>>();
        let testable = revisions
            .iter()
            .filter(|revision| !skipped.contains(&revision.id()))
            .collect::<Vec<_>>();
        if testable.is_empty() {
            let mut commits = candidates.into_iter().collect::<Vec<_>>();
            commits.sort_unstable();
            return Ok(BisectOutcome::OnlySkipped { commits });
        }
        if revisions.len() == 1 && revisions[0].id() == state.bad {
            return Ok(BisectOutcome::Found { commit: state.bad });
        }
        let weights = bisect_weights(&revisions, &candidates, options.max_merge_ancestor_visits)?;
        let total = revisions.len();
        let selected = testable
            .into_iter()
            .enumerate()
            .max_by_key(|(index, revision)| {
                let weight = weights[&revision.id()];
                (weight.min(total - weight), std::cmp::Reverse(*index))
            })
            .map(|(_, revision)| revision)
            .ok_or_else(|| Error::InvalidRepository("bisect has no testable commit".into()))?;
        let weight = weights[&selected.id()];
        let remaining = weight.max(total - weight).saturating_sub(1);
        let outcome = BisectOutcome::Test {
            commit: selected.id(),
            remaining,
            estimated_steps: estimate_steps(remaining),
        };
        self.select_bisect_commit(selected.id(), options, committer)?;
        Ok(outcome)
    }

    /// Restore the pre-bisect HEAD and remove every bisect ref/state file.
    ///
    /// # Errors
    /// Returns an error for inactive state, checkout conflicts, malformed start
    /// data, ref races, or storage failures. Cleanup occurs only after restore.
    pub fn reset_bisect(&self, committer: &Signature, max_object_size: usize) -> Result<()> {
        let state = self.bisect_state()?;
        let no_checkout = self.filesystem().exists(&self.git_path("BISECT_HEAD"))?;
        let already_at_start = matches!(
            self.read_reference("HEAD")?.target(),
            ReferenceTarget::Symbolic(name)
                if name.as_str().strip_prefix("refs/heads/") == Some(state.start.as_str())
        );
        if no_checkout || already_at_start {
            return self.clean_bisect_state();
        }
        if let Ok(branch) = ReferenceName::branch(&state.start) {
            self.switch_branch(
                branch
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&state.start),
                &SwitchOptions {
                    force: false,
                    max_object_size,
                },
                committer,
            )?;
        } else {
            let id = state
                .start
                .parse::<ObjectId>()
                .map_err(|_| Error::InvalidRepository("invalid BISECT_START".into()))?;
            self.switch_detached(
                id,
                &SwitchOptions {
                    force: false,
                    max_object_size,
                },
                committer,
            )?;
        }
        self.clean_bisect_state()
    }

    fn select_bisect_commit(
        &self,
        id: ObjectId,
        options: &BisectOptions,
        committer: &Signature,
    ) -> Result<()> {
        if options.no_checkout {
            self.write_atomic(Path::new("BISECT_HEAD"), format!("{id}\n").as_bytes())?;
        } else {
            self.switch_detached(
                id,
                &SwitchOptions {
                    force: false,
                    max_object_size: options.graph.max_object_size,
                },
                committer,
            )?;
            remove_if_exists(self, "BISECT_HEAD")?;
        }
        self.write_atomic(
            Path::new("BISECT_EXPECTED_REV"),
            format!("{id}\n").as_bytes(),
        )
    }

    fn clean_bisect_state(&self) -> Result<()> {
        let refs = self
            .references()?
            .into_iter()
            .filter(|reference| reference.name().starts_with("refs/bisect/"))
            .map(|reference| (reference.name().to_owned(), reference.target().clone()))
            .collect::<Vec<_>>();
        for (name, target) in refs {
            let name = ReferenceName::new(name)?;
            match target {
                ReferenceTarget::Direct(id) => self.delete_reference(&name, id)?,
                ReferenceTarget::Symbolic(target) => {
                    self.delete_symbolic_reference(name.as_str(), &target)?;
                }
            }
        }
        for file in [
            "BISECT_ANCESTORS_OK",
            "BISECT_EXPECTED_REV",
            "BISECT_HEAD",
            "BISECT_LOG",
            "BISECT_NAMES",
            "BISECT_RUN",
            "BISECT_TERMS",
            "BISECT_FIRST_PARENT",
            "BISECT_START",
        ] {
            remove_if_exists(self, file)?;
        }
        Ok(())
    }

    fn bisect_start_label(&self) -> Result<String> {
        match self.read_reference("HEAD")?.target() {
            ReferenceTarget::Symbolic(name) => Ok(name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(name.as_str())
                .to_owned()),
            ReferenceTarget::Direct(id) => Ok(id.to_string()),
        }
    }

    fn read_bisect_text(&self, name: &str) -> Result<String> {
        let bytes = self.read_git_file(name)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::InvalidRepository(format!("{name} is not UTF-8")))?
            .trim();
        if text.contains(['\0', '\n', '\r']) {
            return Err(Error::InvalidRepository(format!("malformed {name}")));
        }
        Ok(text.to_owned())
    }

    fn append_bisect_log(&self, mark: BisectMark, id: ObjectId) -> Result<()> {
        let term = match mark {
            BisectMark::Good => "good",
            BisectMark::Bad => "bad",
            BisectMark::Skip => "skip",
        };
        let mut log = match self.read_git_file("BISECT_LOG") {
            Ok(log) => log,
            Err(Error::NotFound(_)) => Vec::new(),
            Err(error) => return Err(error),
        };
        log.extend_from_slice(format!("# {term}: [{id}]\n").as_bytes());
        self.write_atomic(Path::new("BISECT_LOG"), &log)
    }
}

fn bisect_weights(
    revisions: &[crate::Revision],
    candidates: &HashSet<ObjectId>,
    max_merge_visits: usize,
) -> Result<HashMap<ObjectId, usize>> {
    let commits = revisions
        .iter()
        .map(|revision| (revision.id(), revision.commit()))
        .collect::<HashMap<_, _>>();
    let mut weights = HashMap::new();
    let mut visits = 0_usize;
    for revision in revisions.iter().rev() {
        let parents = revision
            .commit()
            .parents()
            .iter()
            .copied()
            .filter(|parent| candidates.contains(parent))
            .collect::<Vec<_>>();
        let weight = match parents.as_slice() {
            [] => 1,
            [parent] => weights[parent] + 1,
            _ => {
                let mut seen = HashSet::new();
                let mut stack = parents;
                while let Some(id) = stack.pop() {
                    visits = visits.checked_add(1).ok_or_else(|| {
                        Error::InvalidRepository("bisect merge visit overflow".into())
                    })?;
                    if visits > max_merge_visits {
                        return Err(Error::InvalidRepository(
                            "bisect exceeds merge ancestor visit limit".into(),
                        ));
                    }
                    if seen.insert(id) {
                        stack.extend(
                            commits[&id]
                                .parents()
                                .iter()
                                .copied()
                                .filter(|parent| candidates.contains(parent)),
                        );
                    }
                }
                seen.len() + 1
            }
        };
        weights.insert(revision.id(), weight);
    }
    Ok(weights)
}

fn estimate_steps(remaining: usize) -> u32 {
    usize::BITS - remaining.leading_zeros()
}

fn parse_pseudoref(name: &str, contents: &[u8]) -> Result<ObjectId> {
    let value = std::str::from_utf8(contents)
        .map_err(|_| Error::InvalidRepository(format!("{name} is not UTF-8")))?
        .trim();
    value
        .parse()
        .map_err(|_| Error::InvalidRepository(format!("malformed {name}")))
}

fn remove_if_exists(repository: &Repository, name: &str) -> Result<()> {
    match repository
        .filesystem()
        .remove_file(&repository.git_path(name))
    {
        Ok(()) | Err(Error::NotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitBuilder, InitOptions, MemoryFileSystem, Tree};

    fn linear_fixture(count: usize) -> (Repository, Vec<ObjectId>, Signature) {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("A U Thor", "author@example.com", 1, 0).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let mut commits = Vec::new();
        for index in 0..count {
            let mut builder = CommitBuilder::new(tree, signature.clone(), signature.clone())
                .message(format!("c{index}\n"));
            if let Some(parent) = commits.last() {
                builder = builder.parent(*parent);
            }
            commits.push(repository.write_commit(&builder.build()).unwrap());
        }
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                *commits.last().unwrap(),
                PreviousValue::MustNotExist,
            )
            .unwrap();
        (repository, commits, signature)
    }

    #[test]
    fn bisects_linear_history_marks_and_finds_first_bad() {
        let (repository, commits, signature) = linear_fixture(7);
        let options = BisectOptions {
            no_checkout: true,
            ..BisectOptions::default()
        };
        let first = repository
            .start_bisect(commits[6], &[commits[0]], &options, &signature)
            .unwrap();
        assert!(matches!(first, BisectOutcome::Test { commit, .. } if commit == commits[3]));
        let next = repository
            .mark_bisect(BisectMark::Bad, None, &options, &signature)
            .unwrap();
        assert!(matches!(next, BisectOutcome::Test { commit, .. } if commit == commits[2]));
        let found = repository
            .mark_bisect(BisectMark::Good, None, &options, &signature)
            .unwrap();
        assert_eq!(found, BisectOutcome::Found { commit: commits[3] });
        assert_eq!(repository.bisect_state().unwrap().bad, commits[3]);
    }

    #[test]
    fn skip_reports_only_skipped_and_reset_restores_branch() {
        let (repository, commits, signature) = linear_fixture(2);
        let options = BisectOptions {
            no_checkout: true,
            ..BisectOptions::default()
        };
        assert_eq!(
            repository
                .start_bisect(commits[1], &[commits[0]], &options, &signature)
                .unwrap(),
            BisectOutcome::Found { commit: commits[1] }
        );
        repository
            .mark_bisect(BisectMark::Skip, Some(commits[1]), &options, &signature)
            .unwrap();
        let outcome = repository.bisect_next(&options, &signature).unwrap();
        assert!(matches!(outcome, BisectOutcome::OnlySkipped { .. }));
        repository.reset_bisect(&signature, 1024).unwrap();
        assert!(repository.bisect_state().is_err());
        assert_eq!(
            repository
                .symbolic_reference("HEAD", false)
                .unwrap()
                .as_str(),
            "refs/heads/main"
        );
    }

    #[test]
    fn checkout_mode_detaches_at_candidate_and_reset_restores_symbolic_head() {
        let (repository, commits, signature) = linear_fixture(5);
        let outcome = repository
            .start_bisect(
                commits[4],
                &[commits[0]],
                &BisectOptions::default(),
                &signature,
            )
            .unwrap();
        let BisectOutcome::Test { commit, .. } = outcome else {
            panic!("expected a test candidate");
        };
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), commit);
        assert!(matches!(
            repository.read_reference("HEAD").unwrap().target(),
            ReferenceTarget::Direct(_)
        ));
        repository.reset_bisect(&signature, 1024).unwrap();
        assert_eq!(
            repository
                .symbolic_reference("HEAD", false)
                .unwrap()
                .as_str(),
            "refs/heads/main"
        );
    }
}
