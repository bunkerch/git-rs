# Linked-worktree pruning

`Repository::prune_worktrees` discovers stale entries below the common Git
directory's `worktrees/` directory without assuming host storage. The default
is a dry run. Set `dry_run` to `false` to remove only the selected
administrative entries; the linked working directory itself is never removed.

```rust
use git_rs::{Repository, WorktreePruneOptions};

fn prune(repository: &Repository) -> git_rs::Result<()> {
    let candidates = repository.prune_worktrees(&WorktreePruneOptions {
        expire_before: 1_700_000_000,
        dry_run: true,
        ..WorktreePruneOptions::default()
    })?;
    for candidate in candidates {
        println!("{}: {:?}", candidate.name(), candidate.reason());
    }
    Ok(())
}
```

The rules follow `worktree.c:should_prune_worktree`: a non-directory entry, a
missing `gitdir` file, or an invalid `gitdir` is immediately eligible. A
`locked` file always protects a directory. When `gitdir` points to a missing
location, the entry is eligible only if its administrative `index` is absent or
its modification time is at or before `expire_before`. `max_worktrees` bounds
the complete scan before any mutation begins.

After eligibility checks, registrations are sorted by their resolved `.git`
path and administrative name. All but the first duplicate are selected; an
entry pointing at the main worktree's common directory is always secondary to
the main worktree and is therefore selected. This mirrors Git's deterministic
duplicate pass.

Relative backlinks work in every adapter. Absolute backlinks written by host
Git are interpreted from the adapter namespace root; use a host adapter rooted
at `/` when inspecting such a repository. No Git executable is invoked.

## Git source comparison

- `worktree.c:should_prune_worktree` defines lock protection, malformed-entry
  handling, backlink resolution, and index-mtime expiry.
- `builtin/worktree.c:prune_worktrees` defines administrative-only removal and
  cleanup of an empty `worktrees` directory.
- `Documentation/git-worktree.adoc` documents portable-device locking and the
  `--expire` behavior.
