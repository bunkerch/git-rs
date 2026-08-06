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
`expire_reflog_unreachable_before` additionally computes a bounded reachable
commit set from the direct reference tip. For `HEAD`, every reference commit is
a root. An old entry is removed when either commit endpoint is unreachable;
non-commit endpoints are retained as in Git's gentle lookup, while a non-commit
direct reference selects Git's expire-all policy.
`prune_stale_reflog_entries` verifies that both endpoints are commits with
complete parent, tree, and blob closure. Null endpoints are valid; non-commit,
missing, corrupt, or wrongly typed endpoints are pruned. Successfully proven
objects are cached across entries and the total verified set is bounded.

`reflogs` discovers `HEAD` and `refs/*` log files in bytewise order with count
and directory-depth limits. `drop_reflog` removes one whole log under its
canonical lock and reports whether it existed.

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
- `reflog.c:reflog_expiry_prepare` and `is_unreachable` define root selection,
  non-commit handling, and reachability-based expiry.
- `reflog.c:keep_entry` and `commit_is_complete` define stale endpoint and
  reachable-object closure validation.
- `builtin/reflog.c:cmd_reflog_list` and `cmd_reflog_drop` define discovery and
  complete-log removal.

The implementation uses safe Rust and the abstract filesystem exclusively, and
adds no dependency.
