# Ignore matching

`IgnoreMatcher` implements Git-style ignore files over repository-relative byte
paths. `Repository::status` uses it while finding untracked files, and
`Repository::add` uses it during recursive staging. All ignore files are read
through the repository's `FileSystem`; memory and custom storage behave the
same as host storage.

```rust
use git_rs::IgnoreMatcher;

let mut ignores = IgnoreMatcher::default();
ignores.add_patterns(b"", b"target/\n*.log\n!important.log\n")?;
assert!(ignores.is_ignored(b"debug.log", false));
assert!(!ignores.is_ignored(b"important.log", false));
# Ok::<(), git_rs::Error>(())
```

`Repository::is_ignored(path, is_directory)` loads `.git/info/exclude`, the
root `.gitignore`, and each `.gitignore` along the path. A lower directory's
rules have higher precedence, and the final matching rule decides the result.
Ignored parent directories stop traversal, matching Git's rule that a negation
cannot re-include a file when its parent directory remains excluded.

Supported syntax includes comments, escaped leading `#` and `!`, negation,
directory-only trailing slashes, root anchoring, trailing-space escaping, `?`,
`*`, `**`, bracket ranges, and inverted bracket classes. Matching is byte-safe
and uses `/` as the repository path separator. The dynamic-programming matcher
bounds work to the pattern/path state space instead of exponential wildcard
backtracking.

Tracked paths are never hidden from worktree status and remain stageable after
a later ignore rule is added. Recursive add skips only ignored untracked paths.
Explicitly adding an ignored untracked file returns `Error::IgnoredPath`;
`Repository::add_with_options` with `AddOptions { force: true }` includes it.

## Storage policy

The repository-local `.git/info/exclude` and per-directory `.gitignore` files
are honored. A host-global `core.excludesFile` is deliberately not opened by
default, because doing so would violate the storage boundary for an in-memory
or remote filesystem. Applications can load an explicitly selected global
source into `IgnoreMatcher` when their storage and trust policy permits it.

## Source correspondence

Rule parsing and precedence were compared with Git's `dir.c` pattern-list and
last-match logic, while wildcard behavior was compared with `wildmatch.c`.
Traversal preserves Git's excluded-parent behavior and tracked-file exception.
The implementation is original safe Rust and neither invokes Git nor links to
Git code.
