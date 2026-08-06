# Checkout index

Repository::checkout_index copies cached index entries through the repository's
FileSystem adapter. It works with memory storage, host storage, and custom
backends without invoking Git or assuming operating-system paths.

~~~rust
use git_rs::CheckoutIndexOptions;

let result = repository.checkout_index(
    &[],
    &CheckoutIndexOptions {
        all: true,
        prefix: b"export/".to_vec(),
        ..CheckoutIndexOptions::default()
    },
)?;
assert!(!result.entries().is_empty());
# Ok::<(), git_rs::Error>(())
~~~

## Selection and safety

Callers choose exact byte paths or set all, but cannot combine both. Existing
destinations are rejected unless force is set. Every selected path, object
kind, byte bound, and destination collision is preflighted before the first
normal worktree write. no_create restricts output to destinations that already
exist.

The prefix is concatenated literally with each index path, matching Git. It is
validated as a relative, non-escaping byte path before use. Regular,
executable, symlink, gitlink, and sparse-directory entries retain their
semantics. Sparse directories expand only when ignore_skip_worktree is enabled.

update_stat refreshes index metadata after successful materialization and
publishes the index atomically. Extended flags and unselected entries are
preserved.

## Conflict stages and temporary files

CheckoutIndexStage selects stage zero, merge base, ours, theirs, or every
conflict stage. AllConflicts implies temporary output. Temporary names are
claimed with FileSystem::write_new, so parallel calls cannot overwrite one
another. Symlink entries become regular temporary files containing link target
bytes, as native Git requires.

CheckoutIndexResult returns every source path, stage, actual destination, and a
temporary flag. Its output field provides the native temp association format;
stage-all uses three fields and a dot for an absent stage.

## Resource limits and source comparison

Options bound selected entries, individual object reads, aggregate bytes, and
temporary-name attempts. Bare repositories are rejected because checkout-index
requires a worktree destination.

The behavior follows builtin/checkout-index.c and
Documentation/git-checkout-index.adoc from upstream Git. Tests cover modes,
prefixes, overwrite preflight, no-create, stat updates, skip-worktree entries,
conflict stages, missing stages, and temporary-file rules. Host compatibility
checks compare exported bytes, executable bits, and symlink targets with native
git checkout-index.

~~~text
cargo run --example checkout_index -- REPOSITORY --all --prefix=export/
~~~
