# Status

`Repository::status` performs the same conceptual comparisons as Git status:

```text
HEAD tree  ->  index  ->  worktree
              staged     unstaged
```

The result is sorted by raw path bytes. Each `StatusEntry` independently reports
an index change and a worktree change as added, modified, deleted, type-changed,
or unmerged. Untracked files are worktree-only entries. An unborn `HEAD` is
treated as an empty tree.

```rust
# use git_rs::{InitOptions, MemoryFileSystem, Repository, StatusOptions};
let repository = Repository::init(
    MemoryFileSystem::new(),
    "project",
    &InitOptions::default(),
)?;
let status = repository.status(&StatusOptions::default())?;
assert!(status.is_clean());
# Ok::<(), git_rs::Error>(())
```

Worktree comparisons hash file or symlink bytes when determining modifications;
stat-cache data is not treated as proof of equality. This prioritizes correctness
for adapters whose timestamps or inode fields are unavailable or have coarse
resolution. Assume-valid and skip-worktree entries retain their Git semantics
and are omitted from worktree comparison.

Gitlink directories are treated as tracked boundaries rather than recursively
reported as untracked content. Submodule HEAD/dirty-state inspection will be
provided by the submodule layer.

## Git source comparisons

- `wt-status.c` organizes index-versus-HEAD and worktree-versus-index changes.
- `diff-lib.c:run_diff_index` defines staged comparison behavior.
- `diff-lib.c:run_diff_files` defines worktree comparison and unmerged stages.
- `dir.c:read_directory` defines untracked traversal boundaries.
- `read-cache.c:ce_match_stat_basic` informs mode and gitlink comparisons.
