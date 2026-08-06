# Writing trees from the index

`Repository::write_current_index_tree` creates canonical Git tree objects from
the repository index. `write_index_tree_with_options` performs the same
operation for an explicitly supplied `Index`. All reads and object writes use
the repository's configured `FileSystem`.

~~~rust
use git_rs::WriteTreeOptions;

let root = repository.write_current_index_tree(&WriteTreeOptions::default())?;
let subtree = repository.write_current_index_tree(&WriteTreeOptions {
    prefix: Some(b"src/".to_vec()),
    ..WriteTreeOptions::default()
})?;
# let _ = (root, subtree);
# Ok::<(), git_rs::Error>(())
~~~

The index must be fully merged. Intent-to-add entries are omitted. By default,
every blob, symlink, and sparse-directory object named by the selected index is
required to exist. Gitlinks are exempt because their commits belong to the
submodule repository. `missing_ok` disables object-existence checks, matching
`git write-tree --missing-ok`.

Prefix selection accepts a directory name with or without its trailing slash,
removes that directory component from the emitted tree, and rejects absent,
absolute, empty, NUL-containing, or traversal-bearing prefixes. Object
existence is checked from loose paths and pack indexes without inflating blob
contents, so runtime is proportional to index and pack-index traversal rather
than tracked file size.

The implementation is compared with `builtin/write-tree.c` and
`cache-tree.c:update_one` from upstream Git. Tests cover root and prefix object
IDs, unmerged indexes, intent-to-add omission, missing-object policy, gitlink
exemption, and invalid prefixes.

~~~text
cargo run --example write_tree -- REPOSITORY
cargo run --example write_tree -- REPOSITORY --prefix=src/
~~~
