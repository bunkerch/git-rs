# Unpacking objects

`Repository::unpack_objects` accepts complete pack bytes, validates them in
quarantine, and expands new objects into Git-compatible loose storage. The
operation never invokes Git and publishes through the configured `FileSystem`.

```rust
use git_rs::UnpackObjectsOptions;

# fn unpack(repository: &git_rs::Repository, pack: &[u8]) -> git_rs::Result<()> {
let report = repository.unpack_objects(
    pack,
    &UnpackObjectsOptions {
        strict: true,
        ..UnpackObjectsOptions::default()
    },
)?;
println!("wrote {} objects", report.written.len());
# Ok(())
# }
```

Validation covers the pack header and trailer, exact zlib boundaries, OFS and
REF delta resolution, delta programs, reconstructed IDs, duplicate objects, and
all configured compressed/inflated size limits. Thin REF deltas may use an
existing repository object as their base.

`dry_run` performs the complete validation without mutation. `strict` also
parses every commit, tree, and annotated tag before publication and verifies
their direct links and expected object types. Gitlinks are boundaries, matching
Git's connectivity behavior. Strict failure therefore leaves every incoming
object quarantined rather than publishing a valid prefix.

Objects already available in loose or packed storage are listed in `existing`
and are not rewritten. Newly published IDs are listed in `written`; `objects`
contains the full incoming object set in deterministic ID order.

The implementation follows `builtin/unpack-objects.c`, `packfile.c`, and
`patch-delta.c`. Unlike the command's recovery mode, the library operation is
transactionally preflighted and returns at the first corrupt input; it never
publishes objects from a pack whose integrity validation failed.

The host example accepts a pack on standard input:

```text
cargo run --example unpack_objects -- repository --strict <objects.pack
```
