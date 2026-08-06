# Diff and unified patches

`Repository::diff_trees` compares two recursively flattened trees; either side
may be `None` for the empty tree. `diff_tree_to_index` compares a tree to a
stage-zero index. Results are byte-path-sorted `DiffEntry` values classified as
added, deleted, modified, type-changed, or renamed.

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

The host example writes a patch to standard output:

```console
cargo run --example diff -- repository old-ref new-ref
```

## Git source comparisons

- `diff-lib.c:diff_tree_oid` and `do_diff_cache` define tree/tree and tree/index
  pairing.
- `diff.c:diffcore_rename` defines delete/add rename pairing and deterministic
  destination selection.
- `xdiff-interface.c` and xdiff's Myers implementation define line edit
  frontiers, hunk context, and no-newline reporting.
- `diff.c:run_diff` defines file, mode, index, rename, and binary headers.

The implementation is legally distinct safe Rust and neither invokes Git nor
uses gitoxide.
