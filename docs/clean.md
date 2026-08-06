# Cleaning untracked worktree content

`Repository::clean` discovers or removes untracked files through the repository's
`FileSystem` adapter. It works with host, memory, and custom storage without
invoking Git or depending on host-only filesystem APIs.

The default is a non-mutating, whole-worktree scan. It preserves ignored paths,
does not select wholly untracked directories, and still descends tracked
directories to find untracked children:

```rust
use git_rs::{CleanOptions, Repository};

fn discover(repository: &Repository) -> git_rs::Result<()> {
    for entry in repository.clean::<&str>(&[], &CleanOptions::default())? {
        println!("{}", String::from_utf8_lossy(entry.path()));
    }
    Ok(())
}
```

Mutation requires both `dry_run: false` and `force: true`. Set `directories` to
select wholly untracked directories. A directory containing `.git` is protected
even with force; removing it additionally requires
`remove_nested_repositories: true`. These independent flags prevent an ordinary
clean from erasing a nested repository.

`CleanIgnoredMode::Respect` implements normal ignore handling,
`CleanIgnoredMode::Include` corresponds to Git's `-x`, and
`CleanIgnoredMode::Only` corresponds to `-X`. `exclude_patterns` applies extra
Git-style patterns like repeated `git clean -e` arguments. Paths supplied to
`clean` are literal repository-relative selections, not host globs.

Discovery completes and is sorted before any removal begins. The returned
`CleanEntry` values are therefore also suitable for previews and audit logs.
Deletion failures can still leave a partially cleaned worktree, as with any
filesystem mutation.

The runnable host-backed example remains a preview unless `--force` is present:

```console
cargo run --example clean -- /path/to/repository --directories
cargo run --example clean -- /path/to/repository --directories --force
cargo run --example clean -- /path/to/repository --ignored-only
```

## Git source comparison

The selection and authority split follows `cmd_clean` in `builtin/clean.c`:
directory selection, `-x`/`-X`, path exclusions, force, and the second-force
protection for nested repositories are represented as typed options. Recursive
directory removal corresponds to `remove_dirs` in that file, while ignore-stack
traversal follows the per-directory exclusion model in `dir.c`. The Rust API
deliberately replaces interactive CLI modes with deterministic discovery that a
caller can inspect before requesting mutation.
