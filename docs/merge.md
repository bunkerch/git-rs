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
Text blobs are materialized with `<<<<<<<`, `=======`, and `>>>>>>>` markers;
binary or non-regular conflicts retain one side in the worktree while keeping
all available stages. Resolve files through `add`, then call `continue_merge`.
`abort_merge` restores the original commit tree and clears merge state.

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
- `xdiff-interface.c` defines textual conflict-marker presentation.
- `commit-reach.c` defines best common ancestors and criss-cross bases.

The Rust code is independently organized around flattened typed trees, bounded
object reads, transactional index/ref writes, and the abstract filesystem. It
does not call Git or use gitoxide.
