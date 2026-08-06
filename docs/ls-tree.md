# Listing trees

`Repository::ls_tree` resolves a tree-ish and returns typed entries in canonical
Git tree order. Paths are byte vectors, so non-UTF-8 filenames round-trip on
every filesystem adapter.

```rust
# use git_rs::{LsTreeOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let entries = repository.ls_tree("HEAD", &LsTreeOptions {
    recursive: true,
    include_object_size: true,
    ..LsTreeOptions::default()
})?;
for entry in entries {
    println!("{:06o} {:?} {}", entry.mode_number(), entry.kind(), entry.id());
}
# Ok(())
# }
```

`recursive` descends all selected directories. Directory entries are hidden
during recursion unless `show_trees` is set; `trees_only && recursive` implies
showing them, matching `git ls-tree -d -r`. Literal nested `paths` trigger only
the descent required to reach them even without global recursion. With
`show_trees`, those ancestor trees are included as Git does.

Blob, executable, and symlink sizes are loaded only when
`include_object_size` is true. Trees and gitlinks retain `None`, corresponding
to Git's `-` size field. Revision parsing, object loads, visited entries, and
tree depth all have explicit bounds.

## Git source comparisons

- `builtin/ls-tree.c:show_recursive` defines global and path-triggered descent.
- `builtin/ls-tree.c:show_tree_common` defines tree hiding, `-d`, `-r`, and
  `-t` interactions.
- `builtin/ls-tree.c:show_tree_long` defines blob-only object sizes.
- `tree.c:read_tree_at` defines depth-first canonical tree traversal.

The implementation is independently structured safe Rust, uses repository
object APIs exclusively, adds no dependency, and invokes no executable.
