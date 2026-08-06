# Hashing objects

`Repository::hash_object` computes the Git object ID for caller-owned bytes and
optionally publishes the loose object. Hash-only mode does not mutate storage.

```rust
use git_rs::{HashObjectOptions, InitOptions, MemoryFileSystem, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    "project",
    &InitOptions::default(),
)?;
let id = repository.hash_object(
    b"hello\n",
    &HashObjectOptions {
        write: true,
        ..HashObjectOptions::default()
    },
)?;
# Ok::<(), git_rs::Error>(())
```

`hash_object_path` and `hash_object_paths` resolve worktree-relative paths and
read them exclusively through the configured `FileSystem`. This works unchanged
with host, memory, or user-defined storage. Batch path calls preserve input order
and require an explicit maximum path count. Each object also has a byte limit.

The default object type is `blob`; set `kind` for tree, commit, or tag framing.
Ordinary mode structurally validates those three types. `literally` corresponds
to Git's `--literally`: it bypasses that validation so malformed canonical
objects can be created for corruption and compatibility testing. It remains an
explicit opt-in and does not weaken ordinary object APIs.

The implementation follows `builtin/hash-object.c` and Git's object framing in
`object-file.c`: SHA-1 covers `<type> <decimal-size>\0<contents>`, while loose
storage contains the zlib-compressed framed bytes in the standard two-character
fanout directory.
