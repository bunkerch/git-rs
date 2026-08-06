# Pruning loose objects

`Repository::prune` discovers or removes eligible loose object data through the
repository's `FileSystem`. It never invokes Git and never removes packfiles.

```rust
use git_rs::{PruneOptions, Repository};

fn preview(repository: &Repository) -> git_rs::Result<()> {
    for entry in repository.prune(&PruneOptions::default())? {
        println!("{:?}: {}", entry.reason(), entry.path().display());
    }
    Ok(())
}
```

The default is a dry run. Full fsck and connectivity traversal complete before
selection. Refs, `HEAD`, reflogs, non-gitlink index entries, and
`additional_roots` protect objects. `PruneReason::Unreachable` means a loose
object has no retention path and satisfies `expire_before` (the boundary is
inclusive). With no threshold, dry-run discovery shows every unreachable loose
object.

`PruneReason::PackedDuplicate` means the same ID was reconstructed and hash
verified from an indexed pack. These loose copies are safe to remove regardless
of age, matching Git's `prune-packed` phase. Set `prune_packed_copies: false` to
retain them. Packed data itself is outside this operation's deletion scope.

Stale `tmp_*` files or directory trees immediately under `objects` and
`objects/pack` are expiry-controlled `PruneReason::Temporary` entries. Other
unrecognized files are preserved.

Mutation requires `dry_run: false`, `force: true`, and an explicit
`expire_before`. The operation acquires `.git/gc.pid`, rediscovers all targets
under that maintenance lock, and removes empty fanout directories after their
objects. Lock contention fails before deletion. Repositories declaring
`extensions.preciousObjects=true` reject both preview and mutation.

`max_objects` and `max_object_size` bound verification and discovery. A caller
coordinating an object transaction should add its IDs to `additional_roots`.

```console
cargo run --example prune -- /path/to/repository
cargo run --example prune -- /path/to/repository \
  --force --expire-before=1700000000
```

## Git source comparison

Reachability, inclusive mtime expiry, loose fanout cleanup, `tmp_*` cleanup, and
the precious-object guard correspond to `cmd_prune`, `prune_object`, and
`prune_tmp_file` in `builtin/prune.c`. Packed-copy removal corresponds to the
`prune_packed_objects` phase called there. git-rs adds explicit mutation
authority and performs repository-wide verification before deletion while
retaining Git's object and directory layout.
