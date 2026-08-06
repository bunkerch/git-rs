# Stashing local changes

The stash API saves and restores index and worktree layers without invoking Git
or assuming host filesystem storage. It uses ordinary Git commit objects and
the `refs/stash` reflog, so host Git and git-rs can consume each other's stash
entries.

```rust
use git_rs::{Repository, Signature, StashApplyOptions, StashPushOptions};

fn save_and_restore(repository: &Repository) -> git_rs::Result<()> {
    let identity = Signature::new("Ada", "ada@example.com", 1_700_000_000, 0)?;
    repository.stash_push(
        &StashPushOptions {
            include_untracked: true,
            message: Some(b"parser experiment".to_vec()),
            ..StashPushOptions::default()
        },
        &identity,
    )?;
    repository.stash_pop(
        0,
        &StashApplyOptions {
            reinstate_index: true,
            ..StashApplyOptions::default()
        },
    )?;
    Ok(())
}
```

## Stored representation

A stash worktree commit has this parent order:

1. the `HEAD` commit on which the stash is based;
2. an index-state commit whose tree exactly represents the saved index;
3. when requested and present, a root commit containing non-ignored untracked
   files.

The worktree commit's own tree contains tracked worktree state, including
unstaged modifications and deletions. `refs/stash` points to the newest entry;
its reflog provides `stash@{n}` ordering. Dropping a non-tip entry rewrites the
remaining reflog predecessor chain instead of leaving discontinuous old IDs.

## Operations and safety

- `stash_push` rejects unborn/bare repositories, unresolved index entries, and
  an empty selected change set. It publishes the stash before cleaning, so a
  cleanup error never loses the saved objects.
- `stashes` returns entries newest first.
- `stash_apply` performs a three-way tree merge between the stash base, current
  tracked worktree, and saved worktree. Conflicts are materialized with index
  stages 1–3.
- By default, restored modifications/deletions are unstaged while newly added
  paths remain staged. `reinstate_index` separately merges the saved index diff
  onto the current index before restoring it.
- Included untracked files are restored only when every destination is absent.
  Tracked stash additions also refuse untracked collisions. All collisions are
  checked before tracked worktree mutation.
- `stash_pop` drops the entry only after a clean apply. A conflicted entry stays
  addressable for recovery.
- `stash_drop` supports any reflog index and atomically moves or deletes the
  stash ref when the newest/last entry is removed.

Object reads are bounded by `max_object_size`. Snapshot paths preserve bytes on
Unix, symlink targets, and executable modes. Ignored files remain outside an
`include_untracked` stash, matching `git stash --include-untracked` rather than
`--all`.

## Example

```console
cargo run --example stash -- /path/to/repository push --include-untracked --message "experiment"
cargo run --example stash -- /path/to/repository list
cargo run --example stash -- /path/to/repository apply 0 --index
cargo run --example stash -- /path/to/repository pop 0
cargo run --example stash -- /path/to/repository drop 0
```

## Git source comparisons

This is independently written Rust code. Behavior and tests correspond to:

- `builtin/stash.c:do_create_stash`, `stash_working_tree`, and
  `save_untracked_files` for the base/index/worktree/untracked commit topology;
- `builtin/stash.c:do_apply_stash` for three-way worktree application, optional
  index reconstruction, conflict retention, and untracked restoration;
- `builtin/stash.c:do_store_stash` for compare-and-swap `refs/stash` updates;
- `builtin/stash.c:do_drop_stash` for reflog-index removal and ref movement;
- `merge-ort.c` for three-way path selection and unmerged index stages;
- `refs/files-backend.c` and `lockfile.c` for lockfile publication rules.
