# Read tree into the index

Repository::read_tree_into_index populates or trivially merges the repository
index from typed object IDs. All object and index access uses the FileSystem
adapter and the resulting index is published atomically.

The current API implements the index-only forms of upstream read-tree:

- emptying the index;
- replacing it from one tree-ish;
- binding one tree under a non-colliding directory prefix;
- one-tree merge with cached stat preservation;
- two-tree fast-forward merge with compatible cached changes carried forward;
- three-tree trivial merge with unresolved paths stored at stages 1, 2, and 3;
- reset, aggressive trivial resolution, and dry-run validation.

Tree-ish inputs may be trees, commits, or annotated tags resolving to either.
Blobs and invalid tag targets are rejected.

~~~rust
use git_rs::ReadTreeOptions;

let result = repository.read_tree_into_index(
    &[base_tree, ours_tree, theirs_tree],
    &ReadTreeOptions {
        merge: true,
        ..ReadTreeOptions::default()
    },
)?;
for path in result.conflicts {
    eprintln!("unmerged: {}", String::from_utf8_lossy(&path));
}
# Ok::<(), git_rs::Error>(())
~~~

Two-tree merging follows the carry-forward table documented by
git-read-tree(1): cached changes already equal to the target survive, entries
equal to the old tree advance to the target, and divergent cached changes make
the entire operation fail before publication.

Three-tree merging intentionally does not perform content merging. Paths that
are equal on both sides, or changed on only one side, collapse to stage zero.
Other paths retain each present base/ours/theirs value at its Git index stage.
Aggressive mode additionally collapses trivial deletions and identical adds.

Options bound tree traversal, object reads, and final index entries. Prefixes
must be relative directory byte strings ending in a slash. Existing index
extensions and the selected index version are preserved.

The implementation follows builtin/read-tree.c, unpack-trees.c, and
Documentation/git-read-tree.adoc from upstream Git. Tests cover replacement,
emptying, binding, cached-change carry-forward, atomic failure, trivial
resolution, and exact conflict stages. Development checks compare native and
git-rs index stage listings.

~~~text
cargo run --example read_tree -- REPOSITORY --merge BASE OURS THEIRS
~~~
