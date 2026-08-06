# Repository maintenance

`Repository::run_maintenance` coordinates native maintenance operations under
one `objects/maintenance.lock`, matching Git's cross-process exclusion point.
All task access remains behind the repository's `FileSystem`; the coordinator
does not start processes or assume host paths.

```rust
use git_rs::{MaintenanceOptions, Repository};

# fn optimize(repo: &Repository) -> git_rs::Result<()> {
let preview = repo.run_maintenance(&MaintenanceOptions::full())?;
assert!(preview.dry_run);

let completed = repo.run_maintenance(&MaintenanceOptions {
    dry_run: false,
    force: true,
    ..MaintenanceOptions::full()
})?;
assert_eq!(completed.outcomes().len(), 3);
# Ok(())
# }
```

`MaintenanceOptions::full()` runs GC first, then rebuilds the commit graph and
multi-pack index from the resulting object layout. This ordering prevents
publishing acceleration metadata immediately before a repack makes it stale.
The default matches Git's manual strategy and selects only GC. Explicit
`tasks` execute exactly in caller order and may contain:

- `Gc`
- `CommitGraph`
- `MultiPackIndex`
- `PackRefs`
- `ReflogExpire`
- `WorktreePrune`

Task options and limits are embedded in `MaintenanceOptions`. Reflog expiry
requires at least one explicit cutoff. Pack-refs maintenance always selects all
packable refs and prunes their loose forms, as Git's maintenance task does. A
MIDX task with no current pack indexes reports `Skipped` instead of creating an
invalid empty MIDX.

Dry runs perform task discovery, validation, and encoding without acquiring the
maintenance lock or changing storage. Every mutating run requires `force=true`.
The lock is removed on success and task failure; a pre-existing lock is never
overwritten or removed. Tasks are individually atomic where their underlying
API supports atomic publication, but successful earlier tasks are deliberately
not rolled back if a later task fails, matching repository maintenance tools.

Run the full strategy on a host repository:

```console
cargo run --example maintenance -- /path/to/repository --apply
```

Without `--apply`, the example is a non-mutating preview.

## Source correspondence

Task ordering and configuration behavior are compared with
`Documentation/git-maintenance.adoc`. The object-database lock and ordered task
loop correspond behaviorally to `maintenance_run_tasks` in `builtin/gc.c`.
Each task delegates to independently implemented Rust library operations; no
Git or gitoxide implementation is copied or invoked.
