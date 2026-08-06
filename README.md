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
Atomic loose-reference consolidation is documented in
[`docs/pack-refs.md`](docs/pack-refs.md).
Loose objects are documented in [`docs/objects.md`](docs/objects.md), and packed
object reads in [`docs/packfiles.md`](docs/packfiles.md).
Verified pack consolidation and safe pruning are documented in
[`docs/repack.md`](docs/repack.md).
Expiry-controlled loose-object pruning is documented in
[`docs/prune.md`](docs/prune.md).
Full object and connectivity verification is documented in
[`docs/fsck.md`](docs/fsck.md).
Bounded loose, packed, duplicate, size, and garbage inventory is documented in
[`docs/count-objects.md`](docs/count-objects.md).
Wire-format primitives are documented in
[`docs/protocol.md`](docs/protocol.md).
Server-side fetch negotiation is documented in
[`docs/upload-pack.md`](docs/upload-pack.md).
Server-side push ingestion is documented in
[`docs/receive-pack.md`](docs/receive-pack.md).
Transport-neutral cloning and fetching are documented in
[`docs/fetch-clone.md`](docs/fetch-clone.md).
In-memory Git bundle v2/v3 creation, parsing, verification, and import are
documented in [`docs/bundles.md`](docs/bundles.md).
Transport-neutral push orchestration is documented in
[`docs/push.md`](docs/push.md).
Typed trees and commits are documented in
[`docs/trees-and-commits.md`](docs/trees-and-commits.md).
Ordered recursive and literal path-filtered tree inspection is documented in
[`docs/ls-tree.md`](docs/ls-tree.md).
Index and worktree inventory compatible with common `git ls-files` modes is
documented in [`docs/ls-files.md`](docs/ls-files.md).
Bounded fixed-string search across tracked worktree, index, and tree content is
documented in [`docs/grep.md`](docs/grep.md).
Deterministic TAR and ZIP export of stored trees is documented in
[`docs/archive.md`](docs/archive.md).
Committing the index, including unborn, amend, merge, and detached behavior, is
documented in [`docs/commit.md`](docs/commit.md).
Index versions 2–4 are documented in [`docs/index.md`](docs/index.md).
Worktree staging and index-to-tree writing are documented in
[`docs/worktree.md`](docs/worktree.md).
Path-level index/worktree restoration is documented in
[`docs/restore.md`](docs/restore.md).
Safe discovery and removal of untracked worktree content is documented in
[`docs/clean.md`](docs/clean.md).
Staged, unstaged, unmerged, and untracked reporting is documented in
[`docs/status.md`](docs/status.md).
Ref-moving operations and their logs are documented in
[`docs/reset-switch-reflog.md`](docs/reset-switch-reflog.md).
Revision walking, ancestry, and merge bases are documented in
[`docs/revisions.md`](docs/revisions.md).
Stateful good/bad/skip revision bisection and midpoint selection are documented
in [`docs/bisect.md`](docs/bisect.md).
Fast-forward, three-way, conflict, continue, and abort behavior is documented
in [`docs/merge.md`](docs/merge.md).
Cherry-pick and revert behavior is documented in
[`docs/replay.md`](docs/replay.md).
Typed tree/index differences and unified patches are documented in
[`docs/diff.md`](docs/diff.md).
Lightweight and annotated tag operations are documented in
[`docs/tags.md`](docs/tags.md).
Git-compatible configuration parsing and atomic editing are documented in
[`docs/config.md`](docs/config.md).
Ignore matching and its integration with status and staging are documented in
[`docs/ignore.md`](docs/ignore.md).
Bounded, resumable commit rebasing is documented in
[`docs/rebase.md`](docs/rebase.md).
DWIM revision names, abbreviations, ancestry, peeling, and tree paths are
documented in [`docs/revisions.md`](docs/revisions.md).
Named remotes and typed fetch/push refspecs are documented in
[`docs/remotes.md`](docs/remotes.md).
Git-compatible stash topology, application, conflicts, untracked capture, and
reflog management are documented in [`docs/stash.md`](docs/stash.md).
Git-compatible object annotations, including fanout notes trees and
transactional updates, are documented in [`docs/notes.md`](docs/notes.md).
Transparent object replacement, raw reads, bounded chains, and replacement ref
transactions are documented in [`docs/replace.md`](docs/replace.md).

To create a host-backed repository with the included example:

```console
cargo run --example init -- my-repository
```
