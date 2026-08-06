# Worktree and add

`Repository::add` reads files through `FileSystem`; it never opens a host path
directly. Regular files become blobs, executable state selects mode `100755`,
and symlink target bytes become mode `120000` blobs without following the link.
Directories are traversed recursively in stable adapter order, while the Git
directory is always excluded.

Adding a directory replaces every index entry below that prefix in one atomic
transaction. This stages both current files and deletions. Object writes happen
before the index lock is published, so the index never points at an object that
the same operation has not finished storing.

```rust
# use git_rs::{InitOptions, MemoryFileSystem, Repository, FileSystem};
# use std::path::Path;
let storage = MemoryFileSystem::new();
let repository = Repository::init(storage.clone(), "project", &InitOptions::default())?;
storage.write(Path::new("project/README.md"), b"hello\n")?;
repository.add("README.md")?;
let tree = repository.write_index_tree(&repository.read_index()?)?;
# Ok::<(), git_rs::Error>(())
```

`write_index_tree` rejects unresolved merge stages and intent-to-add entries.
It constructs nested trees without flattening directory boundaries, preserving
blob, executable, link, gitlink, and sparse-tree modes.

`checkout_tree` expands a tree recursively and updates the worktree plus index.
Before changing anything it hashes affected worktree files and rejects modified
tracked files, tracked deletions that would be replaced, untracked destination
paths, and untracked file/directory obstructions. Entries unchanged between the
current index and target preserve local modifications. `CheckoutOptions::force`
requests exact materialization instead.

## Removing tracked paths

`Repository::remove` implements the content-safety rules of `git rm` for one or
more literal repository paths. Selection is completed for every path before
mutation; a directory prefix requires `RemoveOptions::recursive`. The returned
paths are sorted in index order and contain every removed tracked leaf.

```rust
# use git_rs::{RemoveOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
repository.remove(
    &["generated"],
    &RemoveOptions {
        recursive: true,
        ..RemoveOptions::default()
    },
)?;
# Ok(())
# }
```

Without `force`, ordinary removal requires each present worktree file to match
the index and each index entry to match `HEAD`. This prevents losing either
local or staged content. A path already absent from the worktree is safe to
remove, including when staged content differs. `cached` leaves the worktree
untouched and permits removal when the index matches either the worktree or
`HEAD`; content therefore remains recoverable in at least one layer.

Unmerged stages are removed together as an intentional conflict resolution.
`skip-worktree` entries require `include_sparse`, unmatched selections fail
unless `ignore_unmatched` is set, and `dry_run` performs the complete selection
and safety preflight without mutation. Populated gitlink directories require
`force` before recursive deletion.

The API takes literal paths rather than command-line pathspec syntax. Library
callers can expand patterns under their own UI rules without hidden shell or
locale behavior.

Host-backed examples are:

```console
cargo run --example add -- /path/to/repository path/to/file
cargo run --example rm -- /path/to/repository path/to/file
cargo run --example rm -- /path/to/repository generated --recursive
cargo run --example rm -- /path/to/repository path/to/file --cached
```

## Moving tracked paths

`Repository::move_path` moves a single literal file, gitlink, or tracked
directory prefix and rewrites all selected index paths in one index
transaction. Unlike removal, a move preserves local and staged changes: file
bytes move as they exist in the worktree, while each index entry keeps its
object ID, mode, stat data, stage, and extended flags at the new path.

```rust
# use git_rs::{MoveOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
repository.move_path("old/module", "new/module", &MoveOptions::default())?;
# Ok(())
# }
```

Directory moves include untracked and ignored files contained below the source,
matching filesystem rename behavior. Because custom adapters are only required
to atomically rename files, git-rs enumerates directories in stable order,
creates the destination hierarchy, and renames each leaf. A leaf-transfer
failure rolls completed leaves back before returning. The source directories
are removed only after all leaves move.

The complete preflight rejects unresolved source entries, a directory moved
inside itself, nonexistent destination parents, file/directory obstructions,
and index prefix collisions. `force` may replace only a regular file or symlink
destination and its exact stage-zero index entry; it cannot merge directories
or erase unresolved stages. `include_sparse` permits an index-only move when
all selected missing entries are marked `skip-worktree`. `dry_run` performs the
same checks without mutation.

The destination is literal. It does not use the CLI convenience that appends a
source basename when the destination names an existing directory; callers can
construct that explicit destination without filesystem-dependent ambiguity.

```console
cargo run --example mv -- /path/to/repository old/path new/path
cargo run --example mv -- /path/to/repository old/path new/path --force
```

## Linked worktrees

`Repository::add_worktree` creates Git's linked-worktree layout without
assuming host storage. The worktree contains a `.git` indirection file, while
the common repository stores per-worktree `HEAD`, index, backlink, and
`commondir` files below `.git/worktrees/<name>`. Objects, refs, packed refs,
reflogs, hooks, and configuration remain shared. Relative links keep the layout
meaningful inside an in-memory or remote storage namespace; the repository
format is upgraded to Git's `extensions.relativeWorktrees` format.

```rust
# use git_rs::{AddWorktreeOptions, Repository, WorktreeTarget};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let linked = repository.add_worktree(
    "topic-work",
    "topic-work",
    &WorktreeTarget::Branch("topic".into()),
    &AddWorktreeOptions::default(),
)?;
assert_eq!(
    linked.resolve_reference("HEAD")?,
    repository.resolve_reference("refs/heads/topic")?,
);
# Ok(())
# }
```

Storage paths passed to the API remain normalized and cannot contain `..`; an
adapter can place the main and linked paths anywhere within a shared namespace.
Each branch may be active in only one worktree. `linked_worktrees` reads the
registrations, and `remove_worktree` refuses staged, unstaged, or untracked
changes unless `force` is set. Removal recursively targets only the registered
worktree and its matching administrative directory.

The host-backed example is:

```console
cargo run --example worktree -- repository linked-path linked-name topic
```

## Locking linked worktrees

Locks protect registrations whose working directory may temporarily disappear,
for example when stored on removable media. The lock reason uses Git's ordinary
`worktrees/<name>/locked` file and is therefore visible to host Git.

```rust
# use git_rs::{RemoveWorktreeOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
repository.lock_worktree("topic-work", Some("portable device"))?;
assert_eq!(
    repository.worktree_lock_reason("topic-work")?.as_deref(),
    Some("portable device"),
);
repository.unlock_worktree("topic-work")?;
# Ok(())
# }
```

Repeat locking and unlocking an unlocked registration are errors. An ordinary
`remove_worktree(name, true)` may bypass dirty-state protection but not a lock.
Call `remove_worktree_with_options` with both `force` and `override_lock` when
the caller has independently authorized both risks. Pruning always retains
locked registrations.

## Repairing externally moved worktrees

When another system moves a linked worktree directory, repair both directions
of Git's registration with its stable administrative name:

```rust
# use git_rs::Repository;
# fn example(repository: &Repository) -> git_rs::Result<()> {
let changed = repository.repair_worktree("topic-work", "new/topic-work")?;
println!("link files changed: {changed}");
# Ok(())
# }
```

The destination must already be a directory. Repair writes the worktree's
`.git` file and the common directory's `worktrees/<name>/gitdir` backlink using
relative paths. Both complete files are staged with create-only lock files
before either is published; if publishing `.git` fails, the old administrative
backlink is restored. An already-correct pair returns `false` without writing.
The operation moves no worktree content and therefore works identically with
memory, host, object-store, and hybrid adapters.

The explicit administrative name avoids guessing ownership from a corrupt path
and lets callers repair a missing `.git` file after their storage layer moves
the worktree.

## Filesystem requirements

Adapters expose no-follow metadata, symlink target reads/creation, executable
state, and portable stat-cache data. Stores without POSIX metadata may return
zero `FileStat` fields; correctness remains hash-based, while adapters that can
provide stat identity enable Git's fast unchanged-file checks.

## Git source comparisons

- `builtin/update-index.c` and `read-cache.c:add_to_index` define worktree-to-index
  mode, stat, and object handling.
- `object-file.c:index_fd` defines symlink target and regular-file hashing.
- `cache-tree.c:write_index_as_tree` defines stage checks and recursive tree
  construction.
- `read-cache.c:ce_match_stat_basic` defines the stat-cache fields used for fast
  worktree comparisons.
- `unpack-trees.c:verify_uptodate` and `verify_absent` define modified-file and
  untracked-path checkout protection.
- `builtin/rm.c:check_local_mod` defines the worktree/index/`HEAD` safety
  matrix, missing-file behavior, and cached-removal exception.
- `builtin/rm.c:cmd_rm` defines all-path preflight, recursive selection,
  unmerged removal, sparse-entry policy, and index publication ordering.
- `builtin/mv.c:cmd_mv` defines controlled-source checks, directory-prefix
  expansion, self-nesting and destination collision rules, sparse handling,
  worktree movement, and index path rewriting.
- `read-cache.c:rename_index_entry_at` defines preservation of index entry
  metadata while changing its path.
- `worktree.c:get_worktrees`, `write_worktree_linking_files`, and
  `builtin/worktree.c:add_worktree` define common-directory routing, relative
  linking files, registration, checkout exclusivity, and removal safety.
- `worktree.c:worktree_lock_reason` and `builtin/worktree.c:lock_worktree` /
  `unlock_worktree` define lock-file creation, trimmed display reasons, and
  state errors.
- `worktree.c:repair_gitfile`, `repair_worktree_at_path`, and
  `write_worktree_linking_files` define bidirectional repair and relative-link
  formatting after external moves.
