# Structured full-history logs

`Repository::log` combines the bounded topological revision walker with typed
tree diffs and optional Git-style patches. It returns `LogEntry` records rather
than preformatted terminal text, so Rust callers can render or index history
without reparsing output.

```rust
# use git_rs::{LogOptions, ObjectId, Repository};
# fn example(repository: &Repository, tip: ObjectId) -> git_rs::Result<()> {
let entries = repository.log(
    &[tip],
    &[],
    &LogOptions {
        paths: vec![b"src".to_vec()],
        show_patch: true,
        ..LogOptions::default()
    },
)?;
for entry in entries {
    println!("{}", entry.id());
}
# Ok(())
# }
```

Path selection is literal and byte-preserving. A selected directory matches
itself and descendants, with component boundaries enforced. A commit is kept
when the selected paths differ from at least one parent; roots compare against
the empty tree. This is Git's full-history model: parentage is not rewritten or
collapsed. `max_count` is applied after path filtering, matching log output
limiting rather than prematurely truncating graph discovery.

Path-filtered records retain their typed `LogParentDiff` changes even without
rendered output. With `show_patch`, root and single-parent commits also carry
patch bytes. Merge patch bytes are suppressed by default, as in ordinary
`git log -p`; enable `diff_merges` for a patch per parent (`git log -m`). Rename
detection and Myers limits come from `DiffOptions`. `max_patch_bytes` bounds the
aggregate rendered output in addition to graph, object, line, and trace bounds.

All reads use the configured abstract filesystem. Logging a bare repository,
an in-memory clone, or a hybrid object/ref adapter follows the same code path.

The host-backed example accepts an optional literal path, `--patch`, and
`--diff-merges`:

```console
cargo run --example log -- repository HEAD src --patch
```

## Git source comparison

- `revision.c` full-history traversal retains parentage while using tree
  differences for path relevance.
- `log-tree.c:log_tree_diff` compares roots against the empty tree and commits
  against selected parents.
- `builtin/log.c` suppresses ordinary merge patches unless a merge-diff format
  such as separate-parent `-m` is selected.
- `diff.c` pathspec filtering applies old and new rename sides; this API uses
  literal component-prefix selection on the corresponding typed paths.
