# Packing references

`Repository::pack_refs` atomically consolidates selected loose refs into Git's
sorted `packed-refs` format through the configured `FileSystem`.

```rust
use git_rs::{PackRefsOptions, Repository};

fn pack_all(repository: &Repository) -> git_rs::Result<()> {
    let result = repository.pack_refs(&PackRefsOptions {
        all: true,
        ..PackRefsOptions::default()
    })?;
    println!("packed {} refs", result.packed().len());
    Ok(())
}
```

The default selection is `refs/tags/*`, matching `git pack-refs` without
`--all`. `all` selects every shared, direct, valid loose ref. Nonempty `include`
patterns replace the tag default; `exclude` patterns always win. Patterns use
Git wildmatch semantics without pathname restrictions, so `*` can span `/`.

Symbolic refs, broken loose refs, lock files, and per-worktree namespaces
(`refs/bisect/`, `refs/worktree/`, and `refs/rewritten/`) are never packed.
Selected target objects are read and verified. Annotated tags are recursively
peeled with configured depth and object-size limits. Existing packed-only
entries—including dangling entries—and their existing peeled lines are
preserved rather than silently discarded.

Selected loose-reference locks are acquired in bytewise name order, followed by
`packed-refs.lock`. The complete merged file is written and atomically renamed
before any loose ref is removed. With the default `prune: true`, each loose ref
is deleted while its lock remains held, then empty ref directories are removed.
A cleanup failure can leave a redundant loose ref, but the packed value is
already durable. `prune: false` publishes the packed values and retains all
loose copies.

```console
cargo run --example pack_refs -- /path/to/repository --all
cargo run --example pack_refs -- /path/to/repository \
  --include='refs/heads/release/*' --exclude='refs/heads/release/private/*'
```

## Git source comparison

Default tags, `--all`, include/exclude precedence, shared-worktree filtering,
symbolic/broken-ref rejection, publication, and post-publication pruning follow
`should_pack_ref` and `files_optimize` in `refs/files-backend.c`. The emitted
header and fully peeled sorted records follow `refs/packed-backend.c`. The Rust
implementation uses adapter lock files and typed object/ref APIs rather than
copying Git's backend transactions.
