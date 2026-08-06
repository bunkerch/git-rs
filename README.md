# git-rs

A memory-safe, storage-agnostic implementation of Git in Rust. No CLI calls, no
libgit linkage, no host filesystem assumption.

## Quick start

```rust
use git_rs::{InitOptions, MemoryFileSystem, Repository};

let storage = MemoryFileSystem::new();
let repository = Repository::init(storage, "example", &InitOptions::default())?;
assert_eq!(repository.read_git_file("HEAD")?, b"ref: refs/heads/main\n");
# Ok::<(), git_rs::Error>(())
```

```console
cargo add git-rs
cargo run --example init -- my-repository
```

## Key features

- **Abstracted storage.** All data flows through the `FileSystem` trait.
  `HostFileSystem` writes a `.git` directory compatible with the `git` CLI.
  `MemoryFileSystem` keeps everything in process memory. Implement the trait to
  route objects to S3, refs to Postgres, etc.
- **Minimal dependencies.** One production dependency: `miniz_oxide` for zlib.
- **Memory safe.** `unsafe_code = "forbid"` across the entire library.
- **No CLI calls.** Pure Rust reimplementation — zero `git` process invocations.
- **419 tests.** 400 unit tests + 18 integration tests that compare every key
  operation against the system `git` binary byte-for-byte.

## Status

Every common Git operation is available as a library call. All tests pass,
clippy is clean at pedantic level, and the crate compiles with stable Rust
edition 2024.

| Area | Docs |
|---|---|
| Architecture & filesystem adapters | [`docs/architecture.md`](docs/architecture.md), [`docs/filesystems.md`](docs/filesystems.md) |
| Repository init, open, config | [`examples/init.rs`](examples/init.rs), [`docs/config.md`](docs/config.md) |
| Objects (blob, tree, commit, tag) | [`docs/objects.md`](docs/objects.md), [`docs/trees-and-commits.md`](docs/trees-and-commits.md), [`docs/tags.md`](docs/tags.md) |
| References, branches, reflog | [`docs/references.md`](docs/references.md), [`docs/reflogs.md`](docs/reflogs.md), [`docs/show-ref.md`](docs/show-ref.md) |
| Index & worktree | [`docs/index.md`](docs/index.md), [`docs/worktree.md`](docs/worktree.md), [`docs/ls-files.md`](docs/ls-files.md) |
| Commit, diff, status | [`docs/commit.md`](docs/commit.md), [`docs/diff.md`](docs/diff.md), [`docs/status.md`](docs/status.md) |
| Merge, rebase, cherry-pick | [`docs/merge.md`](docs/merge.md), [`docs/rebase.md`](docs/rebase.md), [`docs/replay.md`](docs/replay.md) |
| Fetch, push, pull, clone | [`docs/fetch-clone.md`](docs/fetch-clone.md), [`docs/push.md`](docs/push.md), [`docs/pull.md`](docs/pull.md) |
| Protocol (upload-pack, receive-pack) | [`docs/upload-pack.md`](docs/upload-pack.md), [`docs/receive-pack.md`](docs/receive-pack.md), [`docs/protocol.md`](docs/protocol.md) |
| Packfiles & repacking | [`docs/packfiles.md`](docs/packfiles.md), [`docs/repack.md`](docs/repack.md), [`docs/multi-pack-index.md`](docs/multi-pack-index.md) |
| Maintenance & GC | [`docs/gc.md`](docs/gc.md), [`docs/maintenance.md`](docs/maintenance.md), [`docs/prune.md`](docs/prune.md), [`docs/fsck.md`](docs/fsck.md) |
| Log, blame, bisect, describe | [`docs/log.md`](docs/log.md), [`docs/blame.md`](docs/blame.md), [`docs/bisect.md`](docs/bisect.md), [`docs/describe.md`](docs/describe.md) |
| Stash, notes, replace, submodules | [`docs/stash.md`](docs/stash.md), [`docs/notes.md`](docs/notes.md), [`docs/replace.md`](docs/replace.md), [`docs/submodules.md`](docs/submodules.md) |
| Apply, format-patch, am | [`docs/apply.md`](docs/apply.md), [`docs/format-patch.md`](docs/format-patch.md), [`docs/am.md`](docs/am.md) |
| Bundle, archive, server-info | [`docs/bundles.md`](docs/bundles.md), [`docs/archive.md`](docs/archive.md), [`docs/server-info.md`](docs/server-info.md) |
| Attributes, ignore, sparse checkout | [`docs/attributes.md`](docs/attributes.md), [`docs/ignore.md`](docs/ignore.md), [`docs/sparse-checkout.md`](docs/sparse-checkout.md) |
| Linked worktrees, restore, clean | [`docs/worktree-prune.md`](docs/worktree-prune.md), [`docs/restore.md`](docs/restore.md), [`docs/clean.md`](docs/clean.md) |
| Signature verification | [`docs/verify-signatures.md`](docs/verify-signatures.md) |

## Examples

101 runnable examples covering every operation are in
[`examples/`](examples/). Each demonstrates a single API with minimal setup:

```console
cargo run --example init -- my-repository
cargo run --example clone_local -- /tmp/mirror
cargo run --example receive_pack -- /tmp/bare-repo
```
