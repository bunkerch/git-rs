# Rebase

`Repository::rebase(upstream, onto, options, committer)` replays the non-merge
commits reachable from the current `HEAD` but not from `upstream` onto `onto`.
Revision discovery is bounded by `RebaseOptions::graph`, commits are applied
oldest-first, and the existing three-way replay engine preserves each original
author and message while using the supplied committer identity.

```rust
use git_rs::{RebaseOptions, RebaseResult, Signature};

let upstream = repository.resolve_reference("refs/remotes/origin/main")?;
let onto = upstream;
let committer = Signature::new("Ada", "ada@example.net", 1_700_000_000, 0)?;
match repository.rebase(upstream, onto, &RebaseOptions::default(), &committer)? {
    RebaseResult::Completed { new, .. } => println!("rebased to {new}"),
    RebaseResult::Conflicted { paths, .. } => println!("resolve {paths:?}"),
    result => println!("{result:?}"),
}
# Ok::<(), git_rs::Error>(())
```

## Resumable state

Conflicts materialize worktree markers and index stages 1–3. Sequencer state is
stored below the common repository directory in `rebase-merge`, using a
`git-rebase-todo`, `done`, `orig-head`, `onto`, `head-name`, `msgnum`, and `end`
layout. Every file is accessed through `FileSystem`, including memory and custom
adapters.

- Resolve conflicts, stage every resolved path, then call
  `continue_rebase`.
- Call `skip_rebase` to restore the pre-commit state and omit the conflicting
  commit before advancing.
- Call `abort_rebase` to restore the original branch or detached `HEAD`, index,
  and worktree.

The sequencer verifies that its next todo item agrees with `CHERRY_PICK_HEAD`
and that `HEAD` did not move during conflict resolution. Todo object IDs and
counters are strictly parsed before mutation.

`RebaseEmpty::Drop` omits patches that make no tree change on the new base;
`RebaseEmpty::Keep` records an empty commit. The result reports replayed and
dropped counts. A direct descendant `onto` uses a fast-forward without creating
sequencer state.

This API implements the normal, non-interactive flattening mode. Merge commits
themselves are omitted while their reachable non-merge commits are replayed;
preserving merge topology and interactive todo editing are separate operation
modes rather than silently approximated here.

## Source correspondence

State transitions and recovery behavior were compared with Git's
`builtin/rebase.c` basic-state writer and `sequencer.c` todo/continue/skip/abort
paths. Commit application delegates to git-rs's independently implemented safe
Rust three-way merge and transactional reference APIs. No Git process or linked
Git implementation is used by the library.
