# Verifying packfiles

`Repository::verify_pack` validates a version-2 `.idx` file and its sibling
`.pack` file, then returns structured verbose output and delta-chain statistics.
Both files are read through the repository's `FileSystem`; the API works with
memory and custom remote adapters without materializing host paths.

```rust
use git_rs::{PackOptions, VerifyPackOptions};

# fn verify(repository: &git_rs::Repository, ids: &[git_rs::ObjectId]) -> git_rs::Result<()> {
let written = repository.write_pack(ids, &PackOptions::default())?;
let report = repository.verify_pack(
    &written.index_path,
    &VerifyPackOptions::default(),
)?;
for object in &report.objects {
    println!("{} {:?} {}", object.id, object.kind, object.size);
}
# Ok(())
# }
```

Verification covers index checksum, fanout and sorting, offset tables, pack
header and trailer checksum, per-entry CRC-32, zlib streams, delta programs,
reconstructed object IDs, and delta-base chains. Reports include object ID,
resolved type and size, packed entry size, offset, chain depth, and immediate
base object ID. `delta_histogram` counts deltified objects by chain depth.

Callers must set limits for pack bytes, object count, individual inflated size,
aggregate inflated size, and delta depth. Defaults are suitable for normal local
repositories; services handling untrusted repositories should lower them.

The behavior is compared with `builtin/verify-pack.c`, which delegates to
`index-pack --verify-stat`, and with `pack-check.c`, `packfile.c`, and
`patch-delta.c` for integrity and delta semantics.
