# Cherry-pick and revert

`Repository::replay_commit` applies one commit's change to the current `HEAD`
(`ReplayKind::CherryPick`) or applies its inverse (`ReplayKind::Revert`). It
reuses the bounded three-way tree and conflict-index machinery used by merge,
and reads and writes exclusively through the repository filesystem adapter.

```rust
# use git_rs::{ReplayKind, ReplayOptions, Repository, Signature};
# fn example(repository: &Repository, commit: git_rs::ObjectId) -> git_rs::Result<()> {
let committer = Signature::new("Example", "example@example.com", 1, 0)?;
let result = repository.replay_commit(
    commit,
    ReplayKind::CherryPick,
    &ReplayOptions::default(),
    &committer,
)?;
# let _ = result;
# Ok(())
# }
```

A cherry-pick uses the selected commit parent as the base and the commit as
theirs. A revert reverses those trees. Root commits use the empty tree. Merge
commits require a one-based `ReplayOptions::mainline` parent; specifying a
mainline for a non-merge is rejected.

Clean replay writes a one-parent commit. Cherry-pick preserves the original
author and message while using the supplied committer; revert uses the supplied
identity and generates Git's `Revert "subject"` message. `no_commit` updates the
index and worktree without creating a commit. An empty replay is rejected unless
`allow_empty` is enabled.

Conflicts write stages 1–3, conflict markers where applicable,
`CHERRY_PICK_HEAD` or `REVERT_HEAD`, `MERGE_MSG`, and `ORIG_HEAD`. After
resolution and `add`, call `continue_replay`. `abort_replay` restores the
original tree and index. Continuation rejects unresolved stages or a `HEAD`
which changed during the replay.

## Git source comparisons

- `sequencer.c:do_pick_commit` defines cherry-pick/revert base direction,
  mainline selection, authorship, messages, and replay-head state.
- `sequencer.c:sequencer_continue` and `sequencer_rollback` define continuation
  and abort safety.
- `merge-ort.c` defines the shared three-way and conflict-stage behavior.

The implementation is independently structured in safe Rust and does not call
Git, link to Git, or use gitoxide.
