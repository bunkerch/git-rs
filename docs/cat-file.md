# Inspecting repository objects

`Repository::cat_file` provides bounded content, pretty, type, size, and
existence queries for loose or packed objects. `cat_file_batch` implements the
default byte protocol used by `git cat-file --batch` and `--batch-check`.

~~~rust
use git_rs::{CatFileMode, ObjectKind};

let result = repository.cat_file(
    object_id,
    CatFileMode::Pretty,
    None,
    64 * 1024 * 1024,
    64 * 1024 * 1024,
)?;
assert!(result.exists);

let tree = repository.cat_file(
    commit_id,
    CatFileMode::Content,
    Some(ObjectKind::Tree),
    64 * 1024 * 1024,
    64 * 1024 * 1024,
)?;
# let _ = tree;
# Ok::<(), git_rs::Error>(())
~~~

Expected-type queries peel annotated tags and can derive a tree from a commit.
Content mode returns exact object bytes. Pretty mode returns blobs, commits, and
tags unchanged, while trees use non-recursive `ls-tree` formatting with
canonical modes and Git C-style path quoting. Existence checks use loose paths,
multi-pack indexes, and ordinary pack indexes without inflating object data.

Batch input contains revision expressions separated by LF. The default output
is `<object-id> <type> <size>`, followed by content and another delimiter when
`contents` is enabled. Missing expressions produce `<expression> missing`.
`nul_terminated` selects NUL for both input and output, corresponding to Git's
`-Z`. Input bytes, request count, inflated object size, and total output bytes
are independently bounded.

The implementation follows `builtin/cat-file.c:cat_one_file`, its default batch
expansion, `tree.c`, and Git's path quoting rules. Tests cover every single
query, expected-type peeling, quoted trees, missing batch requests, newline and
NUL framing, output limits, and packed-only objects. Native checks compare
pretty tree bytes and both default batch protocols exactly.

~~~text
cargo run --example cat_file -- REPOSITORY -p OBJECT
printf 'HEAD\nmissing\n' | cargo run --example cat_file -- REPOSITORY --batch
~~~
