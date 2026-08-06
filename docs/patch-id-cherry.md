# Patch identity and cherry classification

`Repository::commit_patch_id` computes Git's stable patch identity for a root
or single-parent commit. It ignores commit metadata and whitespace, disables
rename detection, hashes binary changes by their blob IDs, and combines
per-file hashes with Git's bytewise addition. Merge commits return `None`.

`Repository::cherry` compares the non-merge commits reachable from a selected
head but not an upstream. Results are oldest first. A `CherryCommit` has `-` as
its marker when an upstream commit has an equivalent patch and `+` otherwise.
An optional limit excludes that commit and its ancestors from the head side.

Both operations read objects through the repository's `FileSystem`; they work
unchanged with memory, host, and custom storage adapters. `CherryOptions`
combines explicit graph, object-size, line-count, and Myers trace limits.

```rust
use git_rs::{CherryOptions, Repository};

# fn classify(repository: &Repository, upstream: git_rs::ObjectId,
#             head: git_rs::ObjectId) -> git_rs::Result<()> {
for commit in repository.cherry(upstream, head, None, &CherryOptions::default())? {
    println!("{} {}", commit.marker(), commit.id());
}
# Ok(())
# }
```

Host-backed examples are available with:

```console
cargo run --example patch_id -- path/to/repository HEAD
cargo run --example cherry -- path/to/repository upstream HEAD
```

The implementation's behavioral comparisons are based on Git's `diff.c`
(`diff_get_patch_id`, `patch_id_consume`, and `flush_one_hunk`), `patch-ids.c`
(`commit_patch_id` and merge exclusion), and `builtin/log.c:cmd_cherry` for
range direction, output order, limit handling, and markers.
