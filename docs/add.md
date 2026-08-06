# Staging worktree changes

`Repository::add_paths` stages multiple literal files or directory prefixes as
one index transaction. It recursively reads the worktree through `FileSystem`,
writes Git blob objects, and atomically publishes the checksum-protected index.

```rust
use git_rs::AddTransactionOptions;

# fn stage(repository: &git_rs::Repository) -> git_rs::Result<()> {
let report = repository.add_paths(
    &["src", "Cargo.toml"],
    &AddTransactionOptions::default(),
)?;
println!("staged {} paths", report.staged.len());
# Ok(())
# }
```

A directory selection updates new and modified descendants and stages tracked
descendants missing from the worktree as deletions. Multiple selections are
deduplicated before publication. Conflict stages for a path are replaced by its
new stage-zero entry, matching normal `git add` resolution behavior.

The transaction supports:

- `force` for ignored untracked files;
- `update_only`, corresponding to `git add -u`;
- `include_removals = false`, corresponding to `--ignore-removal`;
- `intent_to_add`, corresponding to `-N`, with a version-3 index when needed;
- an executable-bit override corresponding to `--chmod=+x` or `--chmod=-x`;
- mutation-free `dry_run`;
- explicit file-size and aggregate path-count bounds.

Ignore evaluation loads `info/exclude` and applicable `.gitignore` files from
the same filesystem adapter. Explicit ignored files return `IgnoredPath` unless
forced; ignored descendants of a selected directory are reported and skipped.
Git metadata and the repository's actual git directory are never traversed.

All selected metadata and contents are preflighted before the first object or
index write. Loose objects are immutable and content addressed; index
publication is the single visibility point for the staged transaction.

The implementation follows `builtin/add.c`, `dir.c`, `read-cache.c`, and
`pathspec.c` for directory selection, tracked deletions, ignore treatment,
intent entries, executable modes, and index replacement. This library API takes
literal repository-relative paths; callers that expose Git's CLI pathspec
language can expand it before calling `add_paths`.
