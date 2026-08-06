# Indexing incoming packs

`Repository::index_pack` validates a complete pack stream, resolves every
object and delta in quarantine, builds a Git version-2 index, and optionally
publishes the pack/index pair through `FileSystem`.

```rust
use git_rs::IndexPackOptions;

# fn index(repository: &git_rs::Repository, pack: &[u8]) -> git_rs::Result<()> {
let report = repository.index_pack(
    pack,
    &IndexPackOptions {
        strict: true,
        ..IndexPackOptions::default()
    },
)?;
println!("indexed {} objects", report.objects.len());
# Ok(())
# }
```

Input validation covers pack framing and checksums, exact zlib streams, object
and aggregate size limits, OFS/REF delta resolution, delta programs, duplicate
IDs, and reconstructed object IDs. Thin REF-delta packs may use repository
objects as bases; the published result is rebuilt as a self-contained pack, so
those external bases are no longer required to read incoming objects.

With `strict`, commits, trees, and annotated tags are parsed and every direct
non-gitlink reference is checked for existence and expected type before any
file is published. `dry_run` performs all validation and index construction but
does not mutate storage.

Optional keep and promisor marker contents are byte preserving. Nonempty marker
messages receive a trailing LF, matching Git. Publication order is pack,
`.keep`/`.promisor` markers, then index. Since repository readers discover packs
through indexes, a pack cannot become visible before its protection markers.
Existing content-addressed files must match byte for byte.

The report contains the rebuilt pack checksum, sorted object IDs, encoded pack
and index sizes, and the paths of published files. All input limits come from
`IncomingPackOptions`; no additional dependencies or host-only paths are used.

The implementation follows `builtin/index-pack.c`, `pack-write.c`,
`packfile.c`, and `patch-delta.c` for quarantine, thin-pack repair, index
construction, integrity verification, marker ordering, and publication.

The example reads a bounded pack stream from standard input:

```text
cargo run --example index_pack -- repository --strict --keep=fetch <objects.pack
```
