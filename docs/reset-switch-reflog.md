# Reflogs, reset, and switch

Direct ref updates can include reflogs through
`update_reference_with_reflog`. The update exclusively locks both the loose ref
and `logs/<ref>`, verifies the caller's expected old value, appends the canonical
line, and publishes the log and ref. Reflog identities preserve timezone
details, including `-0000`.

```text
<old-id> <new-id> Name <email> <timestamp> <timezone> TAB <message> LF
```

`Repository::reset` supports the three primary modes:

- `Soft`: move the current branch or detached HEAD only.
- `Mixed`: also rebuild the index from the target commit tree.
- `Hard`: force checkout of the target tree and replace the index.

Branch and HEAD reflogs receive `reset: moving to ...` entries. Resetting an
unborn symbolic HEAD creates its branch with a null old ID.

`switch_branch` uses protected checkout, then makes HEAD symbolic to the target
branch and records the transition. `switch_detached` writes the commit ID
directly to HEAD. Both support a force option through `SwitchOptions`.

## Git source comparisons

- `refs/files-backend.c` defines ref and reflog lock-file publication.
- `refs.c:refs_update_ref` defines expected-old compare-and-swap behavior.
- `reflog.c` and `refs/files-backend.c:files_reflog_iterator_begin` define
  canonical reflog parsing and iteration.
- `builtin/reset.c` defines soft, mixed, and hard layer changes.
- `builtin/checkout.c` and `builtin/checkout--worker.c` define branch and detached
  HEAD switching around unpack-tree protection.

These APIs operate on repository storage only through `FileSystem`; no command
execution or host-specific ref path is involved.
