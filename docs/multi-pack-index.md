# Multi-pack indexes

`git-rs` can generate and consume Git multi-pack-index files at
`objects/pack/multi-pack-index` without bypassing the repository's `FileSystem`.
This avoids opening and binary-searching every pack index for each packed-object
lookup when a repository contains many packs.

```rust
use git_rs::{MultiPackIndexOptions, Repository};

# fn update(repo: &Repository) -> git_rs::Result<()> {
let report = repo.write_multi_pack_index(&MultiPackIndexOptions::default())?;
println!("indexed {} objects in {} packs", report.objects, report.packs);
let midx = repo.read_multi_pack_index(report.bytes + 1, 1_000_000, 100_000_000)?;
assert_eq!(midx.len(), report.objects);
# Ok(())
# }
```

The writer scans `objects/pack` through the adapter, requires every selected
version-2 `.idx` to have its corresponding `.pack`, validates index checksums,
deduplicates object IDs, and publishes a SHA-1 MIDX v1 using lock-and-rename.
Pack names and object IDs are sorted bytewise, making output deterministic on
storage systems without modification timestamps. `preferred_pack` can name an
`.idx` whose copies win duplicate selection. Offsets above `0x7fffffff` use the
`LOFF` chunk.

The reader accepts non-incremental MIDX versions 1 and 2. Before exposing any
location it checks the file checksum, header, aligned chunk table, pack-name
encoding, fanout, OID order, table sizes, pack IDs, large-offset references, and
caller limits. Unknown optional chunks such as `BTMP` and `RIDX` are skipped.
Incremental MIDX chains (`BASE`) and SHA-256 repositories are rejected rather
than partially interpreted.

Packed-object lookup caches a parsed MIDX. It verifies the selected object and
offset against the named pack's ordinary `.idx` before reading pack bytes. A
missing/stale named pack falls back to scanning ordinary pack indexes; malformed
or contradictory metadata is reported.

For a host-backed repository:

```console
cargo run --example multi_pack_index -- /path/to/repository
git -C /path/to/repository multi-pack-index verify
```

Use `--read` to validate and inventory a MIDX written by Git. These commands are
interoperability checks only; library code never launches Git.

## Source correspondence

The implementation follows the format specified by Git's
`Documentation/gitformat-pack.adoc` and compares behavior with the chunk
validation, object-location, duplicate ordering, and writing code in `midx.c`,
`midx-write.c`, and `midx.h`. It is an independent Rust implementation and does
not copy gitoxide.
