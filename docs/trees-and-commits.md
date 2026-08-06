# Trees and commits

Trees preserve filenames as bytes rather than forcing UTF-8. `Tree::new` rejects
empty, slash-containing, NUL-containing, and duplicate names, then applies Git's
directory-aware byte ordering. The five canonical entry modes are represented by
`EntryMode`: regular blob, executable blob, symbolic link, tree, and gitlink.

```rust
use std::str::FromStr;
use git_rs::{EntryMode, ObjectId, Tree, TreeEntry};

let blob = ObjectId::from_str("ce013625030ba8dba906f756967f9e9ca394464a")?;
let tree = Tree::new(vec![TreeEntry::new(
    EntryMode::Blob,
    b"hello.txt".to_vec(),
    blob,
)?])?;
# Ok::<(), git_rs::Error>(())
```

Commit objects preserve parent order, arbitrary message bytes, and unknown or
multiline headers such as signatures. `Signature` stores the signed Unix
timestamp offset and distinguishes Git's unknown-timezone `-0000` marker from
`+0000`, because changing either byte changes the commit ID.

```rust
use git_rs::{CommitBuilder, Signature};
# use std::str::FromStr;
# use git_rs::ObjectId;
# let tree = ObjectId::from_str("4b825dc642cb6eb9a060e54bf8d69288fbee4904")?;
let identity = Signature::new("A U Thor", "author@example.com", 1_700_000_000, 0)?;
let commit = CommitBuilder::new(tree, identity.clone(), identity)
    .message(b"initial commit\n".to_vec())
    .build();
# Ok::<(), git_rs::Error>(())
```

## Git source comparisons

- `tree.c:base_name_compare` defines ordering, including the virtual `/` suffix
  used when one entry is a directory.
- `tree-walk.c:decode_tree_entry` defines the `<mode> <name>NUL<raw-oid>` layout.
- `object.h:canon_mode` defines canonical file, executable, link, directory, and
  gitlink modes.
- `commit.c:parse_commit_buffer` defines mandatory tree and parent headers.
- `commit.c:write_commit_tree` defines canonical header order and message
  separation.
- `ident.c:split_ident_line` defines identity, timestamp, and timezone fields.

Parsing is byte-bounded by `Repository::read_tree` and `read_commit`, which use
the same caller-selected decompression ceiling as other loose objects.

The examples can create an empty commit and inspect a loose commit/tree pair:

```console
cargo run --example commit -- my-repository "message"
cargo run --example show_commit -- my-repository <commit-id>
```
