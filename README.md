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
Bounded resolved reference inventory and annotated-tag dereferencing are
documented in [`docs/show-ref.md`](docs/show-ref.md).
Bounded reflog reads and transactional deletion/expiration are documented in
[`docs/reflogs.md`](docs/reflogs.md).
Atomic loose-reference consolidation is documented in
[`docs/pack-refs.md`](docs/pack-refs.md).
Loose objects are documented in [`docs/objects.md`](docs/objects.md), and packed
object reads in [`docs/packfiles.md`](docs/packfiles.md).
Verified pack consolidation and safe pruning are documented in
[`docs/repack.md`](docs/repack.md).
Expiry-controlled loose-object pruning is documented in
[`docs/prune.md`](docs/prune.md).
Repository-wide lock-scoped garbage collection is documented in
[`docs/gc.md`](docs/gc.md).
Git-compatible, storage-agnostic commit-graph generation and validation is
documented in [`docs/commit-graph.md`](docs/commit-graph.md).
Cached, Git-compatible multi-pack indexing is documented in
[`docs/multi-pack-index.md`](docs/multi-pack-index.md).
Ordered repository-wide performance maintenance is documented in
[`docs/maintenance.md`](docs/maintenance.md).
Cone and non-cone sparse working trees are documented in
[`docs/sparse-checkout.md`](docs/sparse-checkout.md).
Full object and connectivity verification is documented in
[`docs/fsck.md`](docs/fsck.md).
Bounded loose, packed, duplicate, size, and garbage inventory is documented in
[`docs/count-objects.md`](docs/count-objects.md).
Wire-format primitives are documented in
[`docs/protocol.md`](docs/protocol.md).
Server-side fetch negotiation for wire protocols v0/v1 and v2 is documented in
[`docs/upload-pack.md`](docs/upload-pack.md).
Dumb/static HTTP reference and pack metadata generation is documented in
[`docs/server-info.md`](docs/server-info.md).
Server-side push ingestion is documented in
[`docs/receive-pack.md`](docs/receive-pack.md).
Transport-neutral cloning and fetching are documented in
[`docs/fetch-clone.md`](docs/fetch-clone.md).
Both protocol v0/v1 and protocol v2 have client transports and in-process
repository adapters.
Depth-limited history and Git-compatible shallow boundaries are documented in
[`docs/shallow.md`](docs/shallow.md).
Transport-neutral fetch-and-integrate pull policy is documented in
[`docs/pull.md`](docs/pull.md).
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
Bounded linked-worktree registration pruning is documented in
[`docs/worktree-prune.md`](docs/worktree-prune.md).
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
Structured full-history logs with literal path selection and bounded patches
are documented in [`docs/log.md`](docs/log.md).
Bounded contributor summaries and Git-compatible identity canonicalization are
documented in [`docs/shortlog-mailmap.md`](docs/shortlog-mailmap.md).
Stateful good/bad/skip revision bisection and midpoint selection are documented
in [`docs/bisect.md`](docs/bisect.md).
Fast-forward, three-way, conflict, continue, and abort behavior is documented
in [`docs/merge.md`](docs/merge.md).
The same engine is exposed as a non-checkout, bare-compatible tree merge with
structured conflict stages.
Cherry-pick and revert behavior is documented in
[`docs/replay.md`](docs/replay.md).
Typed tree/index differences and unified patches are documented in
[`docs/diff.md`](docs/diff.md).
Bounded validation and application of unified patches is documented in
[`docs/apply.md`](docs/apply.md).
Oldest-first Git-compatible email patch-series generation is documented in
[`docs/format-patch.md`](docs/format-patch.md).
Stateful email patch ingestion with continue, skip, and abort recovery is
documented in [`docs/am.md`](docs/am.md).
Lightweight and annotated tag operations are documented in
[`docs/tags.md`](docs/tags.md).
Bounded nearest-tag/reference naming is documented in
[`docs/describe.md`](docs/describe.md).
Bounded line attribution across history and exact renames is documented in
[`docs/blame.md`](docs/blame.md).
Stable patch identity and patch-equivalent cherry classification are documented
in [`docs/patch-id-cherry.md`](docs/patch-id-cherry.md).
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
Gitlink submodule discovery, initialization, status, and transport-neutral
checkout updates are documented in [`docs/submodules.md`](docs/submodules.md).

To create a host-backed repository with the included example:

```console
cargo run --example init -- my-repository
```
