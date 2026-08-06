# Dumb HTTP server metadata

`Repository::update_server_info` publishes the metadata consumed by Git's
static or "dumb" HTTP transport. Repository contents can then be exposed by an
ordinary object/file server without running upload-pack.

```rust
use git_rs::{Repository, ServerInfoOptions};

fn publish(repository: &Repository) -> git_rs::Result<git_rs::ServerInfoReport> {
    repository.update_server_info(&ServerInfoOptions::default())
}
```

The operation generates two Git-compatible files through the repository's
`FileSystem` adapter:

- `info/refs` contains every resolved `refs/*` name in bytewise order. An
  annotated tag is followed by `<peeled-id>\t<name>^{}`; lightweight tags have
  no peeled line.
- `objects/info/packs` contains `P pack-<sha1>.pack` for each local pack and a
  final blank line.

Every advertised ref object is read and verified. Annotated tag chains are
bounded and fully peeled. Pack filenames must be content-addressed SHA-1 names,
their matching `.idx` files must exist, and indexes are checksum/layout
validated. Like Git, this inventory operation does not inflate every packed
object; object integrity belongs to `fsck` and pack ingestion, keeping routine
publication proportional to refs plus index bytes.

When an existing pack inventory remains valid, its order is retained and new
packs are appended in bytewise order. If it references a removed or malformed
pack, the list is regenerated deterministically. Unchanged bytes are not
republished unless `force` is set. Each changed file uses lock-and-rename;
`dry_run` performs all reads, validation, generation, and comparisons without
mutation. A stale `info/rev-cache` is removed during a mutating run.

`ServerInfoOptions` bounds references, reference recursion, tag peeling, object
size, pack count, and each pack index. `ServerInfoReport` reports ordinary and
peeled refs, pack count, would-change flags, and rev-cache cleanup.

## Git source comparison

The formats and update policy follow `server-info.c`: `add_info_ref()` emits
resolved refs and annotated-tag peel records, `write_pack_info_file()` emits
`P` records plus the blank terminator, valid old pack order is retained, and
`update_server_info()` removes obsolete `info/rev-cache`. Rust publication uses
the same adapter-neutral atomic write primitive as other repository metadata.

For a host repository:

```console
cargo run --example update_server_info -- /srv/repositories/project.git
```
