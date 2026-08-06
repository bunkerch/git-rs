# Committing the index

`Repository::commit_index` turns the stage-zero index into trees, writes a
commit, and advances the current branch or detached `HEAD`. Every read and
write goes through the repository's `FileSystem`; the operation works unchanged
with host, memory, and user-defined storage adapters.

```rust
use git_rs::{CommitOptions, Repository, Signature};

fn commit_staged(repository: &Repository) -> git_rs::Result<git_rs::ObjectId> {
    let author = Signature::new("Ada", "ada@example.com", 1_700_000_000, 0)?;
    repository.commit_index(
        b"Implement object negotiation\n",
        &author,
        &author,
        &CommitOptions::default(),
    )
}
```

The API deliberately receives author and committer identities from the caller.
It does not inspect environment variables, invoke an editor, run hooks, or read
the host clock. Applications can obtain identities from any configuration and
remain deterministic in tests.

## Behavior and safety

- An unborn symbolic `HEAD` creates a root commit and its branch.
- A normal commit makes the current commit its first parent.
- `amend` reuses the current commit's parents. The passed author remains
  explicit, allowing either preservation or reset by the caller.
- Every valid line of `MERGE_HEAD` becomes a parent. Duplicate parent IDs are
  removed, and merge state is cleared only after publication succeeds.
- Unresolved stages and intent-to-add entries are rejected by index-to-tree
  construction.
- An unchanged ordinary tree is rejected unless `allow_empty` is set. Merge
  and amend commits may retain the tree, matching their semantic purpose.
- Active cherry-pick, revert, or rebase state is rejected so callers use the
  dedicated continuation APIs and preserve their author/message rules.
- The current `HEAD` target and expected commit are checked while locks are
  held. Branch publication uses compare-and-swap and updates both branch and
  `HEAD` reflogs. Detached commits lock and compare `HEAD` directly.
- `max_object_size` bounds every parent commit decoded during validation.

Commit objects may be written before a later ref transaction fails. Such an
object is unreachable and harmless, matching Git's content-addressed object
model; no ref points to it after a failed compare-and-swap.

## Host-backed example

First stage content with Git or the library, then commit the existing index:

```console
cargo run --example commit -- /path/to/repository "subject and message"
cargo run --example commit -- /path/to/repository "replacement" --amend
cargo run --example commit -- /path/to/repository "empty marker" --allow-empty
```

## Git source comparisons

This is independently written Rust code. Its behavioral tests correspond to
these upstream Git contracts:

- `builtin/commit.c:prepare_index` prepares and validates the index used for a
  commit.
- `builtin/commit.c:cmd_commit` rejects unresolved entries, evaluates whether
  a commit is committable, reads every `MERGE_HEAD`, constructs parents, writes
  the commit, updates `HEAD`, and removes merge state.
- `builtin/commit.c:commit_index_files` publishes the locked index state.
- `refs/files-backend.c` and `lockfile.c` define compare-and-swap ref locking
  and atomic lockfile publication.
- `commit.c:commit_tree_extended` defines canonical commit construction and
  ordered parent headers.
