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
- `worktree.c:get_worktrees`, `write_worktree_linking_files`, and
  `builtin/worktree.c:add_worktree` define common-directory routing, relative
  linking files, registration, checkout exclusivity, and removal safety.
