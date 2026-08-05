# Loose objects

`Repository::write_object` supports all four fundamental object types: blob,
tree, commit, and annotated tag. It constructs the canonical Git byte stream:

```text
<type> <decimal byte length> NUL <content>
```

The complete stream is hashed with the crate's safe Rust SHA-1 implementation,
compressed as zlib data, and published atomically at
`objects/ab/cdef...`. Object identifiers are derived from bytes and cannot be
selected by a caller.

```rust
use git_rs::{InitOptions, MemoryFileSystem, ObjectKind, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    "project",
    &InitOptions::default(),
)?;
let id = repository.write_object(ObjectKind::Blob, b"hello\n")?;
assert_eq!(id.to_string(), "ce013625030ba8dba906f756967f9e9ca394464a");

let object = repository.read_object(id, 1024)?;
assert_eq!(object.kind(), ObjectKind::Blob);
assert_eq!(object.data(), b"hello\n");
# Ok::<(), git_rs::Error>(())
```

Reads always require a caller-selected decompressed-size ceiling. They validate
the zlib stream, canonical type and decimal length, actual content length, and
the SHA-1 against the requested object ID. This prevents a corrupt or misplaced
file from being returned as trusted object data and limits decompression bombs.

## Dependency policy

Git requires zlib/DEFLATE interoperability. The only direct runtime dependency
is `miniz_oxide`, a safe Rust implementation with no C linkage. SHA-1, object
framing, validation, fanout paths, and storage transactions are implemented
inside `git-rs`. SIMD features are intentionally not enabled because this crate
forbids relying on unsafe code for its memory-safety boundary.

## Git source comparisons

- `odb/source-loose.c:write_object_file` and `write_loose_object` define hash
  calculation, zlib encoding, and loose-object publication.
- `object-file.c:format_object_header` defines canonical header formatting.
- `object-file.c:parse_loose_header` and `unpack_loose_rest` define strict header
  and decompressed-size validation.
- `object-file.c:fill_loose_path` defines two-hex fanout directory paths.
- `block-sha1/sha1.c` and the FIPS SHA-1 vectors are comparison inputs for the
  independent Rust SHA-1 implementation.

Host-backed interoperability examples mirror `git hash-object -w` and
`git cat-file` without invoking either command:

```console
printf 'hello\n' | cargo run --example hash_object -- my-repository
cargo run --example cat_file -- my-repository <object-id>
```
