# Diff and unified patches

`Repository::diff_trees` compares two recursively flattened trees; either side
may be `None` for the empty tree. `diff_tree_to_index` compares a tree to a
stage-zero index. Results are byte-path-sorted `DiffEntry` values classified as
added, deleted, modified, type-changed, or renamed.

`Repository::diff_files` compares tracked index entries to the worktree, while
`Repository::diff_index` compares a tree to either the cached index or tracked
worktree state. Untracked files are intentionally excluded, matching Git's
plumbing commands.

```rust
# use git_rs::{DiffOptions, Repository};
# fn example(repository: &Repository, old: git_rs::ObjectId, new: git_rs::ObjectId) -> git_rs::Result<()> {
let options = DiffOptions::default();
for entry in repository.diff_trees(Some(old), Some(new), &options)? {
    let patch = repository.render_patch(&entry, &options)?;
    # let _ = patch;
}
# Ok(())
# }
```

Exact rename detection pairs deletions and additions with identical mode and
object ID. When several paths share an object, path-prefix/suffix affinity gives
stable deterministic pairing. Similarity-based inexact rename scoring is kept
separate from exact detection so callers never pay to load blob contents for
ordinary structural diffs.

`render_patch` emits Git-style file headers, mode/index metadata, unified text
hunks, no-final-newline markers, exact-rename headers, and binary markers for
NUL-containing blobs and gitlinks. Its line edit script uses iterative Myers
frontiers. `max_lines`, `max_trace_cells`, and `max_object_size` explicitly
bound attacker-controlled memory, algorithmic work, and object inflation.

All comparisons operate on object and index values through `Repository`, so
memory and custom storage adapters behave identically to host repositories.

Layer comparisons return `LayerDiffEntry::Change` for ordinary pairs and a
lossless `Unmerged` record containing base, ours, theirs, and current worktree
values for unresolved paths. `LayerDiffOptions::stage` selects one merge stage
for `diff_files` when a two-way comparison is desired. Assume-valid and
skip-worktree entries avoid filesystem reads. Regular files, executable modes,
symlinks, deletions, and nested-repository gitlinks are compared without writing
objects. File inflation and result counts are explicitly bounded.

The host example writes a patch to standard output:

```console
cargo run --example diff -- repository old-ref new-ref
cargo run --example diff_layers -- repository --files
cargo run --example diff_layers -- repository --cached HEAD
```

## Git source comparisons

- `diff-lib.c:diff_tree_oid` and `do_diff_cache` define tree/tree and tree/index
  pairing.
- `diff-lib.c:run_diff_files` and `run_diff_index` define index/worktree layer
  selection, missing paths, merge stages, and tracked-only behavior.
- `diff.c:diffcore_rename` defines delete/add rename pairing and deterministic
  destination selection.
- `xdiff-interface.c` and xdiff's Myers implementation define line edit
  frontiers, hunk context, and no-newline reporting.
- `diff.c:run_diff` defines file, mode, index, rename, and binary headers.

The implementation is legally distinct safe Rust and neither invokes Git nor
uses gitoxide.
