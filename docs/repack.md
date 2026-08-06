# Repacking repositories

`Repository::repack` consolidates a verified object set into a deterministic,
Git-compatible pack through the repository's `FileSystem`. It uses no process,
host-filesystem, or libgit APIs.

```rust
use git_rs::{RepackOptions, Repository};

fn repack(repository: &Repository) -> git_rs::Result<()> {
    let result = repository.repack(&RepackOptions {
        prune_loose: true,
        ..RepackOptions::default()
    })?;
    println!("packed {} objects", result.packed_objects);
    Ok(())
}
```

Before constructing output, repack performs the full bounded verification
described in [fsck.md](fsck.md). The normal selection contains objects reachable
from refs, `HEAD`, reflogs, index entries, and `additional_roots`. Unreachable
objects remain where they are unless `include_unreachable` is enabled.

`cruft` provides Git's safe alternative to mixing every object into one pack.
Reachable objects go into the ordinary replacement pack; unreachable objects go
into a separate pack with a checksum-protected `pack-*.mtimes` file. The table
is ordered exactly like the corresponding index. Timestamps are preserved from
loose metadata, an existing validated cruft table, or the containing pack mtime,
taking the newest copy when an object appears in multiple places.

```rust
# use git_rs::{RepackOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let result = repository.repack(&RepackOptions {
    cruft: true,
    prune_loose: true,
    delete_redundant_packs: true,
    ..RepackOptions::default()
})?;
println!("packed {} objects", result.packed_objects);
# Ok(())
# }
```

`cruft_expire_before` omits unreachable objects whose preserved timestamp is at
or before the supplied Unix time. Expiration is explicit; without it, no cruft
object is lost. `CruftMtimes::parse` validates the magic, version, SHA-1 format,
object count, corresponding pack checksum, and metadata checksum before an old
cruft table influences retention.

The operation publishes the content-addressed `.pack` first and its `.idx`
second, then rereads every indexed object from that specific new pack. Only
after this succeeds may `prune_loose` remove selected loose copies. A failure
during later cleanup can leave redundant storage, but cannot make a selected
object unavailable through the new pack.

`delete_redundant_packs` is deliberately rejected unless either
`include_unreachable` or `cruft` is enabled. Both modes account for every valid
unexpired object before deletion. Old indexes are removed before their pack
data so new readers cannot discover a half-removed pair; matching stale
`.mtimes`, reverse-index, bitmap, and promisor sidecars are removed last. Packs
carrying a `.keep` file and all of their sidecars are retained. Deletion is
explicit and disabled by default.

`dry_run` validates the entire repository and reports the selection size without
publishing or deleting anything. `max_objects` and `PackOptions::max_object_size`
bound repository-wide work. Empty repositories do not receive useless empty
packs.

```console
cargo run --example repack -- /path/to/repository --dry-run
cargo run --example repack -- /path/to/repository --prune-loose
cargo run --example repack -- /path/to/repository \
  --all-objects --prune-loose --delete-redundant-packs
cargo run --example repack -- /path/to/repository \
  --cruft --prune-loose --delete-redundant-packs
```

## Git source comparison

The reachability versus all-object selection corresponds to the `pack-objects`
mode construction in `builtin/repack.c`. The new-pack-before-old-pack deletion
ordering follows its `names`/`rollback` publication discipline, expressed here
through immutable content-addressed files and the adapter's atomic rename. Pack
construction itself is documented in [packfiles.md](packfiles.md) and is a
native Rust implementation rather than a port of Git or gitoxide.

The cruft split and grace-period behavior follow
`Documentation/gitformat-pack.adoc`'s cruft-pack design. `pack-write.c` defines
the `MTME` header, lexicographic timestamp table, pack checksum, and trailing
metadata checksum; `pack-mtimes.c` defines validation and indexed lookup.
