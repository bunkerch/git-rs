# Merge

`Repository::merge` combines a commit with the current `HEAD` without invoking
Git or bypassing the repository filesystem. It detects up-to-date and
fast-forward cases through the bounded revision graph, or performs a three-way
tree merge from the best common ancestor.

```rust
# use git_rs::{MergeOptions, Repository, Signature};
# fn example(repository: &Repository, topic: git_rs::ObjectId) -> git_rs::Result<()> {
let committer = Signature::new("Example", "example@example.com", 1, 0)?;
let result = repository.merge(topic, &MergeOptions::default(), &committer)?;
# let _ = result;
# Ok(())
# }
```

Independent path changes resolve automatically, including additions,
deletions, executable-mode changes, symlinks, and gitlinks. A clean diverged
merge writes a two-parent commit and moves the symbolic branch or detached
`HEAD` with reflogs. `FastForwardMode::Only` rejects divergence;
`FastForwardMode::Never` creates a merge commit even when a fast-forward is
possible. `no_commit` leaves a clean merged index and worktree with merge state
for `continue_merge`.

Conflicting paths receive Git index stages 1 (base), 2 (ours), and 3 (theirs).
Text blobs are merged at line-region granularity: independent edits within the
same file combine cleanly, identical edits are deduplicated, and only
overlapping changes receive `<<<<<<<`, `=======`, and `>>>>>>>` markers. Binary
or non-regular conflicts retain one side in the worktree while keeping all
available stages. `max_text_merge_lines` and `max_diff_trace_cells` bound line
inventory and Myers trace storage. Resolve files through `add`, then call
`continue_merge`.
`abort_merge` restores the original commit tree and clears merge state.

Exact rename pairing is identity-based and bounded by
`max_rename_comparisons`. A rename on one side is aligned with edits on the
original path from the other side. Identical destination renames coalesce;
rename/delete and divergent rename/rename cases retain Git-compatible conflict
stages and result-tree paths. Each `MergeTreeStage` carries its own `path`
because stages 1, 2, and 3 can legitimately have different names.

## Non-checkout tree merges

`Repository::merge_tree` is the library equivalent of modern
`git merge-tree --write-tree`. It accepts two commits, discovers and combines
their best merge bases, and writes a top-level result tree without reading or
changing `HEAD`, refs, the index, the worktree, or merge-state files. It also
works in bare repositories and on any `FileSystem` adapter.

```rust
# use git_rs::{MergeTreeOptions, ObjectId, Repository};
# fn example(repository: &Repository, ours: ObjectId, theirs: ObjectId) -> git_rs::Result<()> {
let result = repository.merge_tree(ours, theirs, &MergeTreeOptions::default())?;
println!("tree={} clean={}", result.tree, result.is_clean());
for conflict in result.conflicts {
    for stage in conflict.stages {
        println!(
            "path={} stage={} mode={:06o} id={}",
            String::from_utf8_lossy(&stage.path), stage.stage, stage.mode, stage.id,
        );
    }
}
# Ok(())
# }
```

An explicit commit or tree `merge_base` permits tree inputs, matching the
low-level mode of the native command. With conflicts, the result tree contains
the materialized working version and the structured conflict list preserves
every available base (stage 1), ours (stage 2), and theirs (stage 3) object.
`allow_unrelated_histories` selects an empty base instead of rejecting commits
without a shared ancestor.

The implementation writes Git-compatible `ORIG_HEAD`, `MERGE_HEAD`, and
`MERGE_MSG` files. It rejects an already-active merge, unrelated histories,
dirty tracked state, branch movement during conflict resolution, resource-limit
violations, and file/directory collisions that require Git's path-renaming
presentation. Multiple merge bases are recursively combined into a virtual
base tree; conflicting virtual-base content is preserved with marker blobs.

## Git source comparisons

- `builtin/merge.c` defines fast-forward policy, merge state, continuation,
  abort, and ref/reflog sequencing.
- `merge-ort.c` and `unpack-trees.c` define three-way path selection and index
  stage semantics.
- `diffcore-rename.c` defines identity matching and rename candidate pairing.
- `xdiff-interface.c` defines textual conflict-marker presentation.
- `xdiff/xmerge.c` defines diff-derived region combination and overlapping
  change conflicts.
- `commit-reach.c` defines best common ancestors and criss-cross bases.
- `builtin/merge-tree.c:real_merge` defines non-checkout inputs, explicit-base
  behavior, result-tree creation, and structured conflict stages.

The Rust code is independently organized around flattened typed trees, bounded
object reads, transactional index/ref writes, and the abstract filesystem. It
does not call Git or use gitoxide.

Run the host-backed plumbing example with object IDs:

```console
cargo run --example merge_tree -- path/to/repository <ours> <theirs>
```
