# Garbage collection

`Repository::gc` coordinates native maintenance under one repository-wide
`gc.pid` lock. It never spawns Git or bypasses the configured `FileSystem`.

```rust
use git_rs::{GcOptions, Repository};

fn collect(repository: &Repository) -> git_rs::Result<()> {
    let report = repository.gc(&GcOptions {
        dry_run: false,
        force: true,
        reflog_expire_before: Some(1_700_000_000),
        reflog_expire_unreachable_before: Some(1_705_000_000),
        worktree_expire_before: Some(1_700_000_000),
        prune_expire_before: Some(1_710_000_000),
        ..GcOptions::default()
    })?;
    println!("packed {} objects", report.repack.unwrap().packed_objects);
    Ok(())
}
```

The stage order follows Git's maintenance dependencies:

1. Pack packable refs and prune their loose forms.
2. Expire reflogs using the union of total-age and unreachable-age policies.
3. Prune expired missing linked-worktree registrations.
4. Repack reachable objects and retain unreachable objects in a validated cruft
   pack, then remove redundant packs and loose copies.
5. Prune expired unreachable loose objects, packed duplicates, and temporary
   object files.

The default is a complete dry run. Mutation requires `force`; loose-object and
worktree pruning only run when their absolute Unix cutoffs are supplied. This
keeps time policy outside the storage library and makes tests deterministic.
Nested option values expose each stage's object, graph, reference, depth, and
count bounds.

A mutating run acquires `gc.pid` before the first stage and removes it after the
last stage or an error. Existing lock content is never overwritten. Individual
ref and reflog transactions retain their narrower locks inside the GC lock.
Stages publish independently in order, as Git does: a later cleanup failure may
leave completed safe maintenance, but cruft publication and verification finish
before any old object pack is removed.

`extensions.preciousObjects` rejects destructive repack or prune stages. Packs
with `.keep` markers remain untouched. Dry-run reflog expiry uses the same
combined predicate as mutation, avoiding double counts for entries matching
both expiry policies.

```console
cargo run --example gc -- /path/to/repository --dry-run \
  --reflog-before=1700000000 --unreachable-reflog-before=1705000000 \
  --worktree-before=1700000000 --prune-before=1710000000
cargo run --example gc -- /path/to/repository --run \
  --worktree-before=1700000000 --prune-before=1710000000
```

## Git source comparison

- `builtin/gc.c:lock_repo_for_gc` defines the repository-wide maintenance lock.
- `gc_foreground_tasks` orders ref packing before reflog expiry.
- `maintenance_task_worktree_prune` runs worktree cleanup before object-database
  optimization.
- `maintenance_task_odb` coordinates repack, cruft retention, and pruning.
- `reflog.c:should_expire_reflog_ent` defines the union of total and unreachable
  expiry policies.
- Cruft format and publication details are documented in
  [repack.md](repack.md).
