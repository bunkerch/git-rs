# Sparse checkout

Sparse checkout reduces a non-bare working tree while retaining the complete
index and object database. `git-rs` stores Git-compatible rules in
`info/sparse-checkout`, worktree-specific settings in `config.worktree`, and
selection state in each index entry's skip-worktree bit. Every path and state
file operation uses the repository's `FileSystem`.

```rust
use git_rs::{Repository, SparseCheckoutOptions};

# fn select(repo: &Repository) -> git_rs::Result<()> {
let report = repo.set_sparse_checkout(
    &[b"src/library".to_vec(), b"docs".to_vec()],
    &SparseCheckoutOptions::default(),
)?;
println!("removed {} clean paths", report.removed.len());

repo.add_sparse_checkout(&[b"examples".to_vec()], &SparseCheckoutOptions::default())?;
repo.reapply_sparse_checkout(&SparseCheckoutOptions::default())?;
repo.disable_sparse_checkout(&SparseCheckoutOptions::default())?;
# Ok(())
# }
```

Cone mode is the default. Rules are literal repository-relative directories.
Files at the root, files immediately inside every ancestor directory, and all
descendants of a selected directory are included, matching Git's cone pattern
set. The persisted pattern file uses Git's canonical `/*`, `!/*/`, ancestor,
and recursive-directory lines. Pattern metacharacters in literal directory
names are escaped.

With `cone=false`, rules use the ordered `.gitignore` byte-pattern grammar.
Positive matches include paths and later negated matches exclude them. This mode
is bounded by both pattern count/bytes and index entries because it can require
O(patterns × paths) work.

Application performs a complete preflight before mutation. Missing included
paths are materialized from stage-zero index objects. Clean excluded paths are
removed and marked skip-worktree. Modified files, unresolved entries,
directories obstructing tracked files, and gitlinks are retained with the bit
cleared. `force=true` permits removing modified excluded files but never
recursively deletes an obstructing directory. A version-2 index is upgraded to
version 3 when extended flags are needed. Disabling clears every skip-worktree
bit, restores missing tracked paths, preserves existing modifications, disables
the worktree config flag, and removes the pattern file.

`dry_run` validates and reports without changing index, worktree, patterns, or
config. `SparseCheckoutReport` distinguishes materialized, removed, and retained
paths so callers do not need to infer safety decisions.

For a host repository:

```console
cargo run --example sparse_checkout -- /path/to/repository set src docs
cargo run --example sparse_checkout -- /path/to/repository list
cargo run --example sparse_checkout -- /path/to/repository disable
```

## Source correspondence

Behavior and persisted state are compared with
`Documentation/git-sparse-checkout.adoc`, the cone-pattern construction in
`builtin/sparse-checkout.c`, and skip-worktree application in `unpack-trees.c`.
The implementation is independent Rust and does not invoke Git or copy
gitoxide.
