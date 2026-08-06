# Building trees from `ls-tree` records

`Repository::mk_tree` parses non-recursive `ls-tree` records and writes
canonical tree objects through the configured filesystem adapter. Its input and
filenames are byte-oriented, so non-UTF-8 repository names round-trip.

~~~rust
use git_rs::MkTreeOptions;

let input = format!("100644 blob {blob_id}\tREADME.md\n");
let tree_ids = repository.mk_tree(input.as_bytes(), &MkTreeOptions::default())?;
assert_eq!(tree_ids.len(), 1);
# Ok::<(), git_rs::Error>(())
~~~

Each record has `mode SP type SP object-id TAB name` form. Input order does not
matter: entries are normalized with Git's directory-aware tree ordering before
the object is written. Noncanonical regular-file permission bits are preserved
in the raw object, while later tree reads canonicalize them as Git does.
Newline mode decodes Git C-style quoted names, including
exact three-digit octal byte escapes. With `nul_terminated`, NUL ends each
record and names remain literal.

By default every referenced object must exist and its actual type must agree
with both the textual type and mode. `missing` permits absent objects but never
permits an existing object with the wrong type. Missing gitlink commits are
always accepted because they belong to the submodule object database.

Batch mode treats an empty record as a tree boundary and returns one object ID
per completed tree. Empty non-batch input creates Git's canonical empty tree;
empty batch input produces no tree. Input bytes, entries per tree, and inflated
object validation are independently bounded by `MkTreeOptions`.

The behavior follows `builtin/mktree.c`, `quote.c:unquote_c_style`, and
`tree.c:base_name_compare` from upstream Git. Tests cover sorting, quoted and
literal names, NUL batches, missing objects, gitlinks, type disagreement, and
resource limits. Native checks compare resulting object IDs for newline, NUL,
batch, and missing-object inputs.

~~~text
git ls-tree TREE | cargo run --example mktree -- REPOSITORY
git ls-tree -z TREE | cargo run --example mktree -- REPOSITORY -z
~~~
