# Removing packed loose duplicates

`Repository::prune_packed` is the focused, reachability-independent companion
to `Repository::prune`. It removes loose object files only when the same object
ID is present in a validated pack.

```rust
use git_rs::PrunePackedOptions;
# fn example(repository: &git_rs::Repository<impl git_rs::FileSystem>) -> git_rs::Result<()> {
let preview = repository.prune_packed(&PrunePackedOptions::default())?;
println!("{} duplicate loose objects", preview.removed.len());

let removed = repository.prune_packed(&PrunePackedOptions {
    dry_run: false,
    ..PrunePackedOptions::default()
})?;
println!("freed {} stored bytes", removed.removed_bytes);
# Ok(())
# }
```

The operation first validates every discovered `.idx` checksum and its paired
pack framing, object count, pack checksum, and index-linked checksum. Only then
does it scan two-hex-digit loose fanouts. This prevents a corrupt pack from
causing deletion of a readable loose copy. Pack count, packed-object count, and
loose-entry count are independently bounded.

Candidate ordering is deterministic by object ID. Mutation removes candidates
and then empty fanout directories through the configured `FileSystem`; no host
filesystem operation or Git process is used. Invalid unrelated loose filenames
are ignored, matching upstream's loose-object iterator.

The implementation follows `prune-packed.c`, `builtin/prune-packed.c`, and
`Documentation/git-prune-packed.adoc`. The host-backed example previews by
default and requires `--write` to remove files:

```text
cargo run --example prune_packed -- repository --write
```
