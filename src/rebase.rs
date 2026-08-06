//! Bounded, resumable commit rebasing over abstract repository storage.

use std::path::Path;

use crate::{
    Error, GraphOptions, ObjectId, ReplayKind, ReplayOptions, ReplayResult, Repository, ResetMode,
    ResetOptions, Result, RevisionWalkOptions, Signature, StatusOptions,
};

const STATE_DIR: &str = "rebase-merge";
const STATE_FILES: [&str; 8] = [
    "orig-head",
    "onto",
    "head-name",
    "git-rebase-todo",
    "done",
    "msgnum",
    "end",
    "dropped",
];

/// Policy for commits whose patch is empty on the new base.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RebaseEmpty {
    /// Omit commits that have no resulting tree change.
    #[default]
    Drop,
    /// Create a commit even when its tree equals its new parent's tree.
    Keep,
}

/// Rebase graph, object, and empty-commit choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebaseOptions {
    pub graph: GraphOptions,
    pub empty: RebaseEmpty,
}

impl Default for RebaseOptions {
    fn default() -> Self {
        Self {
            graph: GraphOptions::default(),
            empty: RebaseEmpty::Drop,
        }
    }
}

/// Outcome of starting or advancing a rebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RebaseResult {
    UpToDate {
        head: ObjectId,
    },
    FastForward {
        old: ObjectId,
        new: ObjectId,
    },
    Completed {
        old: ObjectId,
        new: ObjectId,
        replayed: usize,
        dropped: usize,
    },
    Conflicted {
        commit: ObjectId,
        paths: Vec<Vec<u8>>,
        completed: usize,
        remaining: usize,
    },
}

#[derive(Clone, Debug)]
struct RebaseState {
    original: ObjectId,
    todo: Vec<ObjectId>,
    done: Vec<ObjectId>,
    dropped: usize,
}

impl Repository {
    /// Replay commits reachable from `HEAD` but not `upstream` onto `onto`.
    ///
    /// Non-merge commits are replayed oldest-first. Merge commits themselves
    /// are omitted, matching a normal non-`--rebase-merges` operation. On a
    /// conflict, state remains under `.git/rebase-merge` for continuation,
    /// skipping, or aborting.
    ///
    /// # Errors
    /// Returns an error for dirty/bare repositories, another operation in
    /// progress, corrupt graph data, graph limit violations, checkout/ref
    /// conflicts, or storage failures.
    pub fn rebase(
        &self,
        upstream: ObjectId,
        onto: ObjectId,
        options: &RebaseOptions,
        committer: &Signature,
    ) -> Result<RebaseResult> {
        if self.work_tree().is_none() {
            return Err(Error::InvalidRepository(
                "rebase requires a worktree".into(),
            ));
        }
        if self.rebase_in_progress()? {
            return Err(Error::InvalidRepository(
                "a rebase is already in progress".into(),
            ));
        }
        if self.filesystem().exists(&self.git_path("MERGE_HEAD"))?
            || self
                .filesystem()
                .exists(&self.git_path("CHERRY_PICK_HEAD"))?
            || self.filesystem().exists(&self.git_path("REVERT_HEAD"))?
        {
            return Err(Error::InvalidRepository(
                "another merge or replay is already in progress".into(),
            ));
        }
        if !self
            .status(&StatusOptions {
                include_untracked: false,
                max_object_size: options.graph.max_object_size,
            })?
            .is_clean()
        {
            return Err(Error::InvalidRepository(
                "cannot rebase with staged or unstaged changes".into(),
            ));
        }
        let original = self.resolve_reference("HEAD")?;
        self.read_commit(upstream, options.graph.max_object_size)?;
        self.read_commit(onto, options.graph.max_object_size)?;
        if original == onto {
            return Ok(RebaseResult::UpToDate { head: original });
        }
        if self.is_ancestor(original, onto, &options.graph)? {
            self.reset_hard(onto, options.graph.max_object_size, committer)?;
            return Ok(RebaseResult::FastForward {
                old: original,
                new: onto,
            });
        }

        let mut todo = self
            .walk_revisions(
                &[original],
                &[upstream],
                &RevisionWalkOptions {
                    graph: options.graph.clone(),
                    ..RevisionWalkOptions::default()
                },
            )?
            .into_iter()
            .filter(|revision| revision.commit().parents().len() <= 1)
            .map(|revision| revision.id())
            .collect::<Vec<_>>();
        todo.reverse();
        if todo.is_empty() {
            self.reset_hard(onto, options.graph.max_object_size, committer)?;
            return Ok(RebaseResult::Completed {
                old: original,
                new: onto,
                replayed: 0,
                dropped: 0,
            });
        }

        self.write_rebase_state(original, onto, &todo)?;
        self.write_atomic(Path::new("ORIG_HEAD"), format!("{original}\n").as_bytes())?;
        self.reset_hard(onto, options.graph.max_object_size, committer)?;
        self.advance_rebase(options, committer)
    }

    /// Continue after conflict resolutions have been staged.
    ///
    /// # Errors
    /// Returns an error if no rebase/conflicted replay is active, unresolved
    /// stages remain, HEAD moved, state is corrupt, or storage fails.
    pub fn continue_rebase(
        &self,
        options: &RebaseOptions,
        committer: &Signature,
    ) -> Result<RebaseResult> {
        let state = self.read_rebase_state()?;
        let expected = *state
            .todo
            .first()
            .ok_or_else(|| Error::InvalidRepository("rebase todo is unexpectedly empty".into()))?;
        let replay = self.read_git_file("CHERRY_PICK_HEAD")?;
        if parse_id(&replay, "CHERRY_PICK_HEAD")? != expected {
            return Err(Error::InvalidRepository(
                "rebase and cherry-pick state disagree".into(),
            ));
        }
        self.continue_replay(options.graph.max_object_size, committer)?;
        self.complete_todo_item(state, expected, false)?;
        self.advance_rebase(options, committer)
    }

    /// Skip the commit currently stopped for conflicts and continue.
    ///
    /// # Errors
    /// Returns an error if no conflicted rebase is active or restoration,
    /// graph, object, reference, or storage operations fail.
    pub fn skip_rebase(
        &self,
        options: &RebaseOptions,
        committer: &Signature,
    ) -> Result<RebaseResult> {
        let state = self.read_rebase_state()?;
        let skipped = *state
            .todo
            .first()
            .ok_or_else(|| Error::InvalidRepository("rebase todo is unexpectedly empty".into()))?;
        let replay = self.read_git_file("CHERRY_PICK_HEAD")?;
        if parse_id(&replay, "CHERRY_PICK_HEAD")? != skipped {
            return Err(Error::InvalidRepository(
                "rebase and cherry-pick state disagree".into(),
            ));
        }
        self.abort_replay(options.graph.max_object_size, committer)?;
        self.complete_todo_item(state, skipped, true)?;
        self.advance_rebase(options, committer)
    }

    /// Restore the original HEAD, index, and worktree and remove rebase state.
    ///
    /// # Errors
    /// Returns an error if no rebase is active or restoration/storage fails.
    pub fn abort_rebase(&self, max_object_size: usize, committer: &Signature) -> Result<()> {
        let state = self.read_rebase_state()?;
        self.reset_hard(state.original, max_object_size, committer)?;
        self.clear_replay_files()?;
        self.clear_rebase_state()
    }

    fn advance_rebase(
        &self,
        options: &RebaseOptions,
        committer: &Signature,
    ) -> Result<RebaseResult> {
        loop {
            let state = self.read_rebase_state()?;
            let Some(target) = state.todo.first().copied() else {
                let new = self.resolve_reference("HEAD")?;
                let result = RebaseResult::Completed {
                    old: state.original,
                    new,
                    replayed: state.done.len().saturating_sub(state.dropped),
                    dropped: state.dropped,
                };
                self.write_atomic(
                    Path::new("ORIG_HEAD"),
                    format!("{}\n", state.original).as_bytes(),
                )?;
                self.clear_rebase_state()?;
                return Ok(result);
            };
            let replay = ReplayOptions {
                allow_empty: options.empty == RebaseEmpty::Keep,
                max_object_size: options.graph.max_object_size,
                ..ReplayOptions::default()
            };
            match self.replay_commit(target, ReplayKind::CherryPick, &replay, committer) {
                Ok(ReplayResult::Committed { .. }) => {
                    self.complete_todo_item(state, target, false)?;
                }
                Ok(ReplayResult::Conflicted { paths }) => {
                    return Ok(RebaseResult::Conflicted {
                        commit: target,
                        paths,
                        completed: state.done.len(),
                        remaining: state.todo.len(),
                    });
                }
                Ok(ReplayResult::Prepared { .. }) => {
                    return Err(Error::InvalidRepository(
                        "rebase unexpectedly prepared an uncommitted replay".into(),
                    ));
                }
                Err(Error::EmptyReplay) if options.empty == RebaseEmpty::Drop => {
                    self.complete_todo_item(state, target, true)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reset_hard(
        &self,
        target: ObjectId,
        max_object_size: usize,
        committer: &Signature,
    ) -> Result<()> {
        self.reset(
            target,
            &ResetOptions {
                mode: ResetMode::Hard,
                max_object_size,
            },
            committer,
        )
    }

    fn rebase_in_progress(&self) -> Result<bool> {
        self.filesystem().exists(&self.git_path(STATE_DIR))
    }

    fn write_rebase_state(
        &self,
        original: ObjectId,
        onto: ObjectId,
        todo: &[ObjectId],
    ) -> Result<()> {
        self.filesystem()
            .create_dir_all(&self.git_path(STATE_DIR))?;
        let head_name = match self.read_reference("HEAD")?.target() {
            crate::ReferenceTarget::Symbolic(name) => name.as_str().as_bytes().to_vec(),
            crate::ReferenceTarget::Direct(_) => b"detached HEAD".to_vec(),
        };
        self.write_state_file("orig-head", format!("{original}\n").as_bytes())?;
        self.write_state_file("onto", format!("{onto}\n").as_bytes())?;
        self.write_state_file("head-name", &[head_name, b"\n".to_vec()].concat())?;
        self.write_state_file("git-rebase-todo", &encode_todo(todo))?;
        self.write_state_file("done", b"")?;
        self.write_state_file("msgnum", b"0\n")?;
        self.write_state_file("end", format!("{}\n", todo.len()).as_bytes())?;
        self.write_state_file("dropped", b"0\n")
    }

    fn read_rebase_state(&self) -> Result<RebaseState> {
        if !self.rebase_in_progress()? {
            return Err(Error::InvalidRepository("no rebase is in progress".into()));
        }
        parse_id(&self.read_state_file("onto")?, "rebase onto")?;
        Ok(RebaseState {
            original: parse_id(&self.read_state_file("orig-head")?, "rebase orig-head")?,
            todo: parse_todo(&self.read_state_file("git-rebase-todo")?)?,
            done: parse_todo(&self.read_state_file("done")?)?,
            dropped: parse_usize(&self.read_state_file("dropped")?, "rebase dropped")?,
        })
    }

    fn complete_todo_item(
        &self,
        mut state: RebaseState,
        target: ObjectId,
        dropped: bool,
    ) -> Result<()> {
        if state.todo.first() != Some(&target) {
            return Err(Error::InvalidRepository(
                "rebase todo changed unexpectedly".into(),
            ));
        }
        state.todo.remove(0);
        state.done.push(target);
        state.dropped += usize::from(dropped);
        self.write_state_file("git-rebase-todo", &encode_todo(&state.todo))?;
        self.write_state_file("done", &encode_todo(&state.done))?;
        self.write_state_file("msgnum", format!("{}\n", state.done.len()).as_bytes())?;
        self.write_state_file("dropped", format!("{}\n", state.dropped).as_bytes())
    }

    fn write_state_file(&self, name: &str, contents: &[u8]) -> Result<()> {
        self.write_atomic(Path::new(STATE_DIR).join(name).as_path(), contents)
    }

    fn read_state_file(&self, name: &str) -> Result<Vec<u8>> {
        self.read_git_file(Path::new(STATE_DIR).join(name))
    }

    fn clear_replay_files(&self) -> Result<()> {
        for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG"] {
            remove_if_exists(self, &self.git_path(name))?;
        }
        Ok(())
    }

    fn clear_rebase_state(&self) -> Result<()> {
        for name in STATE_FILES {
            remove_if_exists(self, &self.git_path(Path::new(STATE_DIR).join(name)))?;
        }
        match self.filesystem().remove_dir(&self.git_path(STATE_DIR)) {
            Ok(()) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn encode_todo(ids: &[ObjectId]) -> Vec<u8> {
    let mut output = Vec::with_capacity(ids.len() * 46);
    for id in ids {
        output.extend_from_slice(format!("pick {id}\n").as_bytes());
    }
    output
}

fn parse_todo(contents: &[u8]) -> Result<Vec<ObjectId>> {
    contents
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value = line.strip_prefix(b"pick ").ok_or_else(|| {
                Error::InvalidRepository("unsupported rebase todo instruction".into())
            })?;
            parse_id(value, "rebase todo")
        })
        .collect()
}

fn parse_id(contents: &[u8], label: &str) -> Result<ObjectId> {
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository(format!("{label} is not ASCII")))?
        .parse()
        .map_err(|_| Error::InvalidRepository(format!("{label} is invalid")))
}

fn parse_usize(contents: &[u8], label: &str) -> Result<usize> {
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| Error::InvalidRepository(format!("{label} is invalid")))
}

fn remove_if_exists(repository: &Repository, path: &Path) -> Result<()> {
    match repository.filesystem().remove_file(path) {
        Ok(()) | Err(Error::NotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, PreviousValue,
        ReferenceName, Tree, TreeEntry,
    };

    fn repository() -> (Repository, MemoryFileSystem, Signature) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Rebaser", "rebaser@example.com", 100, 0).unwrap();
        (repository, filesystem, signature)
    }

    fn commit(
        repository: &Repository,
        parent: Option<ObjectId>,
        files: &[(&[u8], &[u8])],
        message: &[u8],
        timestamp: i64,
    ) -> ObjectId {
        let signature = Signature::new("Author", "author@example.com", timestamp, 0).unwrap();
        let entries = files
            .iter()
            .map(|(name, contents)| {
                let blob = repository
                    .write_object(crate::ObjectKind::Blob, contents)
                    .unwrap();
                TreeEntry::new(EntryMode::Blob, name.to_vec(), blob).unwrap()
            })
            .collect();
        let tree = repository.write_tree(&Tree::new(entries).unwrap()).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature).message(message);
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }

    fn checkout(repository: &Repository, target: ObjectId) {
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                target,
                PreviousValue::Any,
            )
            .unwrap();
        repository
            .checkout_tree(
                repository.read_commit(target, 4096).unwrap().tree(),
                &crate::CheckoutOptions {
                    force: true,
                    max_object_size: 4096,
                },
            )
            .unwrap();
    }

    fn options() -> RebaseOptions {
        RebaseOptions {
            graph: GraphOptions {
                max_commits: 100,
                max_object_size: 4096,
            },
            ..RebaseOptions::default()
        }
    }

    #[test]
    fn replays_linear_commits_oldest_first_onto_new_base() {
        let (repository, filesystem, committer) = repository();
        let base = commit(
            &repository,
            None,
            &[(b"a", b"base"), (b"b", b"base"), (b"up", b"base")],
            b"base\n",
            1,
        );
        let first = commit(
            &repository,
            Some(base),
            &[(b"a", b"one"), (b"b", b"base"), (b"up", b"base")],
            b"first\n",
            2,
        );
        let second = commit(
            &repository,
            Some(first),
            &[(b"a", b"one"), (b"b", b"two"), (b"up", b"base")],
            b"second\n",
            3,
        );
        let onto = commit(
            &repository,
            Some(base),
            &[(b"a", b"base"), (b"b", b"base"), (b"up", b"upstream")],
            b"upstream\n",
            4,
        );
        checkout(&repository, second);

        let result = repository
            .rebase(base, onto, &options(), &committer)
            .unwrap();
        let RebaseResult::Completed {
            old,
            new,
            replayed,
            dropped,
        } = result
        else {
            panic!("unexpected rebase result");
        };
        assert_eq!((old, replayed, dropped), (second, 2, 0));
        let new_second = repository.read_commit(new, 4096).unwrap();
        assert_eq!(new_second.message(), b"second\n");
        let new_first = new_second.parents()[0];
        let new_first = repository.read_commit(new_first, 4096).unwrap();
        assert_eq!(new_first.parents(), [onto]);
        assert_eq!(new_first.message(), b"first\n");
        assert_eq!(filesystem.read(Path::new("repo/a")).unwrap(), b"one");
        assert_eq!(filesystem.read(Path::new("repo/b")).unwrap(), b"two");
        assert_eq!(filesystem.read(Path::new("repo/up")).unwrap(), b"upstream");
        assert!(!repository.rebase_in_progress().unwrap());
    }

    #[test]
    fn conflict_can_continue_and_abort_restores_original_head() {
        let (repository, filesystem, committer) = repository();
        let base = commit(&repository, None, &[(b"file", b"base\n")], b"base\n", 1);
        let local = commit(
            &repository,
            Some(base),
            &[(b"file", b"local\n")],
            b"local\n",
            2,
        );
        let onto = commit(
            &repository,
            Some(base),
            &[(b"file", b"upstream\n")],
            b"upstream\n",
            3,
        );
        checkout(&repository, local);
        let result = repository
            .rebase(base, onto, &options(), &committer)
            .unwrap();
        assert!(matches!(result, RebaseResult::Conflicted { commit, .. } if commit == local));
        assert!(repository.rebase_in_progress().unwrap());

        filesystem
            .write(Path::new("repo/file"), b"resolved\n")
            .unwrap();
        repository.add("file").unwrap();
        let completed = repository.continue_rebase(&options(), &committer).unwrap();
        assert!(matches!(
            completed,
            RebaseResult::Completed { replayed: 1, .. }
        ));
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"resolved\n"
        );

        checkout(&repository, local);
        repository
            .rebase(base, onto, &options(), &committer)
            .unwrap();
        repository.abort_rebase(4096, &committer).unwrap();
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), local);
        assert_eq!(filesystem.read(Path::new("repo/file")).unwrap(), b"local\n");
        assert!(!repository.rebase_in_progress().unwrap());
    }

    #[test]
    fn skips_conflicts_and_drops_patch_already_on_upstream() {
        let (repository, filesystem, committer) = repository();
        let base = commit(&repository, None, &[(b"file", b"base")], b"base\n", 1);
        let conflicting = commit(
            &repository,
            Some(base),
            &[(b"file", b"local")],
            b"conflict\n",
            2,
        );
        let later = commit(
            &repository,
            Some(conflicting),
            &[(b"file", b"local"), (b"later", b"yes")],
            b"later\n",
            3,
        );
        let onto = commit(
            &repository,
            Some(base),
            &[(b"file", b"upstream")],
            b"onto\n",
            4,
        );
        checkout(&repository, later);
        repository
            .rebase(base, onto, &options(), &committer)
            .unwrap();
        let result = repository.skip_rebase(&options(), &committer).unwrap();
        assert!(matches!(
            result,
            RebaseResult::Completed {
                replayed: 1,
                dropped: 1,
                ..
            }
        ));
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"upstream"
        );
        assert_eq!(filesystem.read(Path::new("repo/later")).unwrap(), b"yes");

        let same_change = commit(
            &repository,
            Some(base),
            &[(b"file", b"upstream")],
            b"duplicate\n",
            5,
        );
        checkout(&repository, same_change);
        let result = repository
            .rebase(base, onto, &options(), &committer)
            .unwrap();
        assert_eq!(
            result,
            RebaseResult::Completed {
                old: same_change,
                new: onto,
                replayed: 0,
                dropped: 1,
            }
        );
    }

    #[test]
    fn fast_forwards_without_leaving_sequencer_state_and_rejects_dirty_input() {
        let (repository, filesystem, committer) = repository();
        let base = commit(&repository, None, &[(b"file", b"base")], b"base\n", 1);
        let descendant = commit(
            &repository,
            Some(base),
            &[(b"file", b"descendant")],
            b"descendant\n",
            2,
        );
        checkout(&repository, base);
        assert_eq!(
            repository
                .rebase(base, descendant, &options(), &committer)
                .unwrap(),
            RebaseResult::FastForward {
                old: base,
                new: descendant
            }
        );
        assert!(!repository.rebase_in_progress().unwrap());
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"descendant"
        );

        checkout(&repository, base);
        filesystem.write(Path::new("repo/file"), b"dirty").unwrap();
        assert!(
            repository
                .rebase(base, descendant, &options(), &committer)
                .is_err()
        );
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), base);
        assert!(!repository.rebase_in_progress().unwrap());
    }
}
