# git-rs

`git-rs` is a memory-safe, storage-agnostic implementation of Git in Rust. It
does not invoke Git, link to libgit, or assume repository data lives on the host
filesystem.

The project is under active development. The initial API establishes the
filesystem boundary and creates Git-compatible repository layouts on either the
host filesystem or in memory.

```rust
use git_rs::{InitOptions, MemoryFileSystem, Repository};

let storage = MemoryFileSystem::new();
let repository = Repository::init(storage, "example", &InitOptions::default())?;
assert_eq!(repository.read_git_file("HEAD")?, b"ref: refs/heads/main\n");
# Ok::<(), git_rs::Error>(())
```

See [`docs/architecture.md`](docs/architecture.md) and
[`docs/filesystems.md`](docs/filesystems.md) for the design and adapter contract.
Reference and branch APIs are covered in
[`docs/references.md`](docs/references.md).
Loose objects are documented in [`docs/objects.md`](docs/objects.md), and packed
object reads in [`docs/packfiles.md`](docs/packfiles.md).
Wire-format primitives are documented in
[`docs/protocol.md`](docs/protocol.md).
Server-side fetch negotiation is documented in
[`docs/upload-pack.md`](docs/upload-pack.md).
Server-side push ingestion is documented in
[`docs/receive-pack.md`](docs/receive-pack.md).
Transport-neutral cloning and fetching are documented in
[`docs/fetch-clone.md`](docs/fetch-clone.md).
Typed trees and commits are documented in
[`docs/trees-and-commits.md`](docs/trees-and-commits.md).
Index versions 2–4 are documented in [`docs/index.md`](docs/index.md).
Worktree staging and index-to-tree writing are documented in
[`docs/worktree.md`](docs/worktree.md).
Staged, unstaged, unmerged, and untracked reporting is documented in
[`docs/status.md`](docs/status.md).
Ref-moving operations and their logs are documented in
[`docs/reset-switch-reflog.md`](docs/reset-switch-reflog.md).

To create a host-backed repository with the included example:

```console
cargo run --example init -- my-repository
```
