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
