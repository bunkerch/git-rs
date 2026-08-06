# Materializing blobs temporarily

`Repository::unpack_file` reads a blob from loose or packed object storage and
creates a collision-exclusive temporary file containing its exact bytes.

```rust
use git_rs::UnpackFileOptions;
# fn example(repository: &git_rs::Repository<impl git_rs::FileSystem>, blob: git_rs::ObjectId) -> git_rs::Result<()> {
let file = repository.unpack_file(blob, &UnpackFileOptions::default())?;
println!("temporary path: {} ({} bytes)", file.path().display(), file.len());

// After the merge/tool consumer is finished:
repository.remove_unpacked_file(&file)?;
# Ok(())
# }
```

Only blob objects are accepted. Object inflation is bounded by
`max_object_size`, so the same policy applies to loose data and reconstructed
pack deltas. The complete blob is validated before any temporary path is
created.

Temporary names begin with `.merge_file_`, include an object-derived component,
and are acquired with `FileSystem::write_new`. Existing names are never
overwritten; `max_name_attempts` bounds collision retries. The returned
`UnpackedFile` has private path construction and exposes its adapter-relative
path and byte length. `remove_unpacked_file` performs explicit cleanup.

Non-bare repositories place files at the worktree root. Bare repositories use
their Git root. This keeps every operation inside the configured filesystem,
including memory and remote adapters, without host temporary directories or
Git subprocesses.

The implementation follows `builtin/unpack-file.c` and
`Documentation/git-unpack-file.adoc`. The host example accepts a full object
ID and prints the created path:

```text
cargo run --example unpack_file -- repository <blob-id>
```
