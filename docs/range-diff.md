# Range diff

`Repository::range_diff` compares two base-exclusive patch series without
invoking Git or depending on host storage. It returns structured rows whose
status is `Equal`, `Changed`, `Dropped`, or `Added`, together with one-based
positions, commit IDs, and the displayed subject.

```rust
use git_rs::{RangeDiffOptions, Repository};

# fn compare(repository: &Repository,
#            old_base: git_rs::ObjectId, old_tip: git_rs::ObjectId,
#            new_base: git_rs::ObjectId, new_tip: git_rs::ObjectId)
#            -> git_rs::Result<()> {
for row in repository.range_diff(
    old_base,
    old_tip,
    new_base,
    new_tip,
    &RangeDiffOptions::default(),
)? {
    println!("{:?}: {:?} -> {:?}", row.status(), row.old_position(), row.new_position());
}
# Ok(())
# }
```

The comparison follows Git's global model:

1. Each non-merge commit is converted to a canonical metadata/message/diff
   record that omits unstable object IDs and hunk line numbers.
2. Identical diffs are paired before similarity work.
3. Every remaining old/new pair receives a unified line-difference cost.
4. Add/drop dummy rows use `diff_lines * creation_factor / 100` (60 by
   default).
5. A minimum-cost assignment chooses correspondences for the entire series,
   avoiding greedy local mismatches.
6. Rows are emitted primarily in new-series order while dropped commits appear
   after their old predecessors.

`RangeDiffOptions` bounds graph discovery, object size, diff lines and trace
cells, pair comparisons, canonical patch bytes, and cost-matrix memory. The
matrix is preflighted before allocation. All repository reads use `FileSystem`.

Run the host-backed example with:

```console
cargo run --example range_diff -- path/to/repository old-base old-tip new-base new-tip
```

The implementation is behaviorally compared with `range-diff.c` (`read_patches`,
`find_exact_matches`, `get_correspondences`, and output ordering) and
`linear-assignment.c` for global assignment. The Rust assignment implementation
is independently written using the standard primal-dual Hungarian formulation.
