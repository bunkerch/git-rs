# Shallow repositories

A shallow repository contains boundary commit IDs in the common Git directory's
`shallow` file. Those commits retain their original parent headers, but graph
walks treat them as roots because their parents are intentionally absent.
Storage uses the repository `FileSystem`; no host path or process is involved.

```rust
use git_rs::{CloneOptions, Repository, ShallowOptions, UploadPackTransport};

# fn clone<T: UploadPackTransport>(transport: &mut T) -> git_rs::Result<()> {
# let storage = git_rs::MemoryFileSystem::new();
let (repository, result) = Repository::clone_from(
    storage,
    "clone",
    transport,
    &CloneOptions { depth: Some(1), ..CloneOptions::default() },
)?;
assert_eq!(
    repository.shallow_commits(&ShallowOptions::default())?,
    result.shallow_commits,
);
# Ok(())
# }
```

Both v0/v1 and v2 upload-pack parse positive absolute depths. The server walks
commit generations breadth-first, includes complete trees and blobs for every
selected commit, stops at repository/client shallow boundaries, and sends
`shallow`/`unshallow` updates before a sideband pack. Object count and size
limits apply throughout.

The client persists boundary changes only after validating and publishing the
pack. `walk_revisions`, ancestry, merge-base, receive connectivity, and fsck
load the declared boundaries so a deliberately absent parent is not treated as
corruption. Commit object reads remain byte-faithful and still expose the
original parent headers.

## Source correspondence

Behavior is compared with `Documentation/gitprotocol-pack.adoc`,
`Documentation/gitprotocol-v2.adoc`, `upload-pack.c:process_deepen`,
`shallow.c:get_shallows_or_depth`, `fetch-pack.c`, and Git's shallow-file
handling. The implementation is independent Rust and does not invoke Git or
copy gitoxide.
