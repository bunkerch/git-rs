# Reference log maintenance

Reference updates can already append Git-compatible reflog records atomically.
The maintenance API adds bounded reads and transactional pruning without an
external process:

```rust
# use git_rs::{ReflogRewriteOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let preview = repository.delete_reflog_entries(
    "refs/heads/main",
    &[1], // refs/heads/main@{1}, counted from newest
    &ReflogRewriteOptions {
        dry_run: true,
        rewrite: true,
        update_reference: true,
        ..ReflogRewriteOptions::default()
    },
)?;
println!("would remove {} entries", preview.removed);
# Ok(())
# }
```

`read_reflog_bounded` returns entries oldest-first while rejecting a log above
the caller's limit. `delete_reflog_entries` accepts zero-based newest-relative
positions. Duplicate and out-of-range selectors are errors.
`expire_reflog_before` removes entries whose committer timestamp is strictly
older than the cutoff, matching Git's `--expire` boundary.

All rewrites lock the reference first, then prepare the log lock. `dry_run`
performs validation and selection but publishes nothing. `rewrite` reconnects
each retained record's old ID to the last retained new ID, beginning with the
null ID. `update_reference` is rejected for symbolic references; for a direct
reference it publishes the last retained new ID after the log. An empty retained
log does not move the reference. Temporary locks are cleaned on every error.

## Git source comparisons

- `builtin/reflog.c:cmd_reflog_delete` and `cmd_reflog_expire` define selector,
  dry-run, rewrite, update-ref, and timestamp modes.
- `refs/files-backend.c:files_reflog_expire` defines reference-first locking and
  log-before-reference publication.
- `refs/files-backend.c:expire_reflog_ent` defines rewrite chaining and last-kept
  tip selection.
- `reflog.c:should_expire_reflog_ent` defines the strict total-expiry boundary.

The implementation uses safe Rust and the abstract filesystem exclusively, and
adds no dependency.
