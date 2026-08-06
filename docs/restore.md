# Restoring paths

`Repository::restore_paths` replaces selected literal files or directory
prefixes in the worktree, index, or both. It uses only the repository
`FileSystem`, so the same operation works in memory or through custom remote
storage adapters.

```rust
use git_rs::{Repository, RestoreOptions, RestoreTarget};

fn discard_staged_and_local_changes(repository: &Repository) -> git_rs::Result<()> {
    repository.restore_paths(
        &["src/parser.rs"],
        &RestoreOptions {
            target: RestoreTarget::Both,
            ..RestoreOptions::default()
        },
    )?;
    Ok(())
}
```

## Source and target layers

- `RestoreTarget::Worktree` changes only worktree files and defaults to the
  current index. This discards unstaged changes while retaining staged content.
- `RestoreTarget::Index` changes only index entries and defaults to `HEAD`. This
  unstages changes without modifying worktree bytes.
- `RestoreTarget::Both` writes both layers and defaults to `HEAD`.
- `source: Some(id)` replaces the default with an explicit commit or tree for
  any target mode.

An unborn `HEAD` supplies an empty source for index/combined restoration, so
newly staged paths can be unstaged. Worktree restoration directly from an index
rejects selected unresolved stages; restoring the index from a tree removes
stages 1–3 and writes a stage-zero entry.

## Selection and safety

Each input is a literal normalized repository path. A directory prefix selects
all current-or-source descendants. Selection is computed across the current
index and source before mutation, which allows both restoring deleted paths and
removing paths absent from the source. Unmatched input fails unless
`ignore_unmatched` is enabled.

Selected tracked modifications are deliberately overwritten. Untracked files,
directories, and parent-file obstructions are rejected before mutation unless
`force` is explicit. Symlink target bytes and executable modes are restored
without following links. Deletions prune only empty parent directories.

Index publication uses the ordinary checksum-protected lockfile transaction.
Entry-dependent extensions such as cache-tree data are invalidated whenever
the index changes. `dry_run` performs source parsing, bounded object reads,
selection, conflict checks, and mode validation without changing either layer.

```console
cargo run --example restore -- /path/to/repository src/parser.rs
cargo run --example restore -- /path/to/repository src --staged
cargo run --example restore -- /path/to/repository src --staged --worktree
cargo run --example restore -- /path/to/repository src --source=<40-hex-object-id>
```

## Git source comparisons

This implementation is independent Rust code. Behavior and tests correspond
to these upstream contracts:

- `builtin/checkout.c:checkout_main` selects worktree by default and makes
  `--staged [--worktree]` default to `HEAD`.
- `builtin/checkout.c:checkout_paths` expands selected source/current paths,
  reads a source tree, updates the index, and dispatches worktree restoration.
- `builtin/checkout.c:checkout_worktree` writes selected entries and reports
  path-level checkout failures.
- `unpack-trees.c:verify_absent` defines protection for untracked file and
  directory obstructions.
- `entry.c:checkout_entry_ca` defines regular, executable, symlink, gitlink,
  and file/directory materialization behavior.
- `read-cache.c` and `lockfile.c` define index locking, checksum publication,
  and invalidation of entry-dependent extensions.
