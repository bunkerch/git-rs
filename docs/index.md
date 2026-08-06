# Index (directory cache)

`Index` reads and writes Git index versions 2, 3, and 4. Paths remain raw bytes,
entries are sorted by path and merge stage, and all fixed-width integers use the
format's network byte order. Version 4 prefix compression uses Git's offset
varint rather than a standard LEB128 encoding.

Each `IndexEntry` carries stat-cache fields, canonical mode, object ID, merge
stage, assume-valid, intent-to-add, and skip-worktree state. Version 3 or 4 is
required when extended flags are present. Index writes use the same exclusive
`index.lock` and atomic rename transaction as Git.

```rust
use std::str::FromStr;
use git_rs::{Index, IndexEntry, IndexVersion, ObjectId, StatData};

let blob = ObjectId::from_str("ce013625030ba8dba906f756967f9e9ca394464a")?;
let entry = IndexEntry::new(
    b"hello.txt".to_vec(),
    0o100_644,
    blob,
    StatData { size: 6, ..StatData::default() },
)?;
let index = Index::new(IndexVersion::V2, vec![entry])?;
# Ok::<(), git_rs::Error>(())
```

The trailing SHA-1 is always verified before entries or extensions are trusted.
Optional extensions are preserved byte-for-byte. Unknown required lowercase
extensions are rejected. The `link` split-index extension is rejected explicitly
until its shared base index can be merged; returning its delta entries as a full
index would be unsafe. Sparse-directory (`sdir`) entries are represented by mode
`040000` and preserved.

Index paths reject absolute paths, empty components, `.`/`..`, NUL, backslash,
and case-insensitive `.git` components. This makes indexes constructed through
the API safe to hand to future checkout code across filesystem adapters.

## Git source comparisons

- `read-cache-ll.h` defines `DIRC`, versions, stages, and extended flags.
- `read-cache.c:ondisk_cache_entry` documents stat and object fields.
- `read-cache.c:create_from_disk` defines v2-v4 decoding and path expansion.
- `read-cache.c:ce_write_entry` defines padding and v4 prefix compression.
- `varint.c` defines Git's decrement-before-continuation varint.
- `read-cache.c:verify_hdr` defines signature, version, and checksum validation.

The examples update an index and inspect any Git-created v2-v4 index:

```console
cargo run --example update_index -- my-repository path/to/file contents 4
cargo run --example ls_files -- my-repository
```
