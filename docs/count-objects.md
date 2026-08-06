# Counting objects

`Repository::count_objects` performs a bounded, read-only inventory through the
`FileSystem` interface. It reports loose object count and stored bytes, valid
pack/index pairs and their entries, loose objects duplicated in a pack, and
garbage with typed reasons and paths.

```rust
# use git_rs::{CountObjectsOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let report = repository.count_objects(&CountObjectsOptions::default())?;
println!("{} loose; {} packed", report.loose_objects, report.packed_objects);
for garbage in &report.garbage {
    eprintln!("{:?}: {}", garbage.reason(), garbage.path().display());
}
# Ok(())
# }
```

`max_loose_entries` and `max_pack_files` bound discovery before attacker-owned
storage can consume unbounded work or memory. Pack indexes are checksum- and
structure-validated with `PackIndex`; invalid pairs are reported as garbage and
are never included in object totals. A `BTreeSet` of packed IDs makes
`prune_packable` an efficient intersection with discovered loose IDs.

Sizes are logical file lengths rather than host allocation blocks. This keeps
results deterministic for memory, object storage, database, and host adapters.
Counts, pack totals, and prune-packable values match Git; byte-to-kilobyte
formatting is left to the caller.

## Git source comparisons

- `builtin/count-objects.c:count_loose` defines loose, size, and packed-loose
  accounting.
- `builtin/count-objects.c:cmd_count_objects` defines pack object/pair totals
  and the verbose report fields.
- `odb/source-packed.c:report_pack_garbage` defines grouping of pack sidecar
  files and incomplete pair classification.
- `packfile.c` and the version-2 index reader define indexed object counts.

The implementation is independently structured safe Rust, has no new
dependencies, and does not invoke Git or another executable.
