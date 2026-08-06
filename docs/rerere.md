# Reusing recorded resolutions

`git-rs` implements Git's reuse-recorded-resolution cache as structured
`Repository` methods. It operates on the real unmerged index and worktree
produced by merge, rebase, cherry-pick, revert, or am, while all cache,
attribute, index, object, and worktree I/O goes through the configured
`FileSystem`.

```rust
use git_rs::{Repository, RerereOptions};

fn resolve_known_conflicts(repository: &Repository) -> git_rs::Result<()> {
    let report = repository.rerere(&RerereOptions {
        autoupdate: true,
        ..RerereOptions::default()
    })?;

    for path in report.reused() {
        println!("reused resolution for {}", String::from_utf8_lossy(path));
    }
    Ok(())
}
```

`rerere` performs one complete lifecycle pass:

- discover regular-file conflicts with index stages 2 and 3;
- normalize conflict markers and compute the Git conflict ID;
- apply every compatible cached variant using a true three-way text merge;
- record unresolved normalized preimages;
- record postimages for paths manually resolved since the prior pass;
- atomically publish `MERGE_RR`; and
- optionally replace unmerged index stages with a stage-zero blob.

The report separates newly recorded preimages, newly recorded resolutions,
reused paths, autostaged paths, and paths still remaining.

## Cache compatibility

The normal filesystem adapter writes Git's layout:

```text
.git/
  MERGE_RR
  rr-cache/<conflict-id>/
    preimage
    postimage
    preimage.<variant>
    postimage.<variant>
```

The conflict ID is SHA-1 over every conflict side pair after discarding the
diff3 base, sorting the two sides bytewise, and NUL-terminating each side.
Surrounding context is deliberately excluded from the ID. Different contexts
for the same conflict therefore use numbered variants, exactly as Git does.
Preimages always have unlabeled LF marker lines; non-marker bytes retain their
original line endings.

Reuse treats the cached preimage as the merge base, the current normalized
conflict as the current side, and the postimage as the other side. A resolution
is published only when this merge is clean. This preserves new surrounding
edits instead of performing unsafe textual replacement.

The `conflict-marker-size` attribute is resolved per path using the library's
attribute engine. Invalid, unset, or non-positive values fall back to seven.
`RerereOptions::attributes` supplies attribute parsing bounds; rerere fixes its
source to worktree-then-index and its requested attribute name.

## Management operations

- `rerere_status` lists paths tracked by `MERGE_RR`.
- `rerere_remaining` combines unresolved tracked paths with conflict types
  rerere cannot safely handle. Successfully reused but unstaged regular
  conflicts are not reported, matching Git.
- `rerere_forget` finds the cached variant which cleanly applies to each
  current conflict, deletes its postimage, refreshes its preimage, and tracks
  it again.
- `rerere_clear` removes current-session state and incomplete preimage-only
  variants while retaining complete reusable resolution pairs.

Deletion resolutions and non-regular stage pairs are intentionally reported as
remaining rather than replayed: upstream Git also punts these cases because
silently discarding a modified side is unsafe.

## Bounds and source comparison

Path count, file size, variants per conflict, nested-conflict depth, merge
output, line count, diff trace cells, and attribute parsing all have explicit
limits. Cache and ordinary conflict traversal are iterative; syntactically
nested conflict hunks additionally consume the explicit depth bound.

The implementation was derived from the behavior in upstream `rerere.c`,
`rerere.h`, `builtin/rerere.c`, and `ll_merge_marker_size()` in `merge-ll.c`.
Unit tests cover normalization, diff3 handling, side ordering, recording,
context-adjusted reuse, autostaging, attributes, forget, clear, and bounds.
`examples/rerere.rs` exposes the lifecycle for a host repository. Twin native
Git and Rust repositories are compared byte-for-byte for `MERGE_RR`, conflict
IDs, preimages, postimages, reused worktree content, and the resolved index.
