# Updating the index

`Repository::update_index` applies a bounded list of typed commands to an
in-memory index snapshot and publishes one checksum-protected index file. It
supports direct cache entries (including merge stages), removal, refresh,
executable changes, assume-unchanged, skip-worktree, intent-to-add, dry runs,
and index format selection.

```rust
use git_rs::{IndexEntry, StatData, UpdateIndexCommand, UpdateIndexOptions};
# fn example(repository: &git_rs::Repository<impl git_rs::FileSystem>, id: git_rs::ObjectId) -> git_rs::Result<()> {
let entry = IndexEntry::new(b"README".to_vec(), 0o100_644, id, StatData::default())?;
repository.update_index(
    &[
        UpdateIndexCommand::CacheInfo(entry),
        UpdateIndexCommand::AssumeUnchanged { path: b"README".to_vec(), value: true },
    ],
    &UpdateIndexOptions::default(),
)?;
# Ok(())
# }
```

Cache entries must reference an existing object of the kind implied by their
mode. Refresh hashes worktree content through the repository's abstract
filesystem and changes stat data only when content and mode still match;
otherwise the path is returned in `needs_update`. Setting an extended flag on
a v2 index automatically promotes it to v3. Existing optional index extensions
are preserved.

The implementation follows `builtin/update-index.c`, `read-cache.c`, and
`Documentation/git-update-index.adoc`. It never invokes Git or assumes host
filesystem storage.
