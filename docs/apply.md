# Applying patches

`Repository::apply_patch` consumes ordinary Git-style unified patches without
invoking Git and without assuming a host filesystem. The same implementation
operates on `MemoryFileSystem`, `HostFileSystem`, or a custom adapter.

```rust
use git_rs::{ApplyOptions, InitOptions, MemoryFileSystem, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    "repo",
    &InitOptions::default(),
)?;
let patch = b"diff --git a/hello b/hello\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/hello\n\
@@ -0,0 +1 @@\n\
+hello\n";
let report = repository.apply_patch(patch, &ApplyOptions::default())?;
assert_eq!(report.created, 1);
# Ok::<(), git_rs::Error>(())
```

Set `check` to validate every file and preimage without mutation. Set `index`
to update the worktree and index together; this additionally requires the
indexed blob and mode to match the worktree preimage. The index lock is acquired
before worktree mutation. Set `reverse` to exchange old/new paths, ranges,
modes, additions, and deletions.

The parser supports multi-file patches, creation, deletion, exact renames,
regular/executable files, symbolic links, quoted Git paths, multiple hunks, and
`No newline at end of file` markers. Binary patch payloads are rejected. Hunk
placement is deliberately exact: both the old line number and every context or
deleted line must match. This deterministic library API does not silently fuzz
or relocate hunks.

All externally controlled allocation dimensions are bounded by `ApplyOptions`:
patch bytes, files, hunks, and resulting file size. Paths pass through the index
path validator, and each existing parent is checked before writes so a patch
cannot escape the abstract worktree through `..` or a symbolic-link parent.

## Git source comparison

The implementation follows the validation-before-mutation structure in
`apply.c`: `check_patch_list()` checks all patches, `check_patch()` validates
preimages, destinations, modes, and unsafe paths, and
`apply_one_fragment()` constructs and matches preimage/postimage line data.
Unlike the command-line porcelain, this API accepts already-selected patch
bytes and therefore has no CLI pathspec, whitespace-warning, reject-file, or
interactive behavior.
