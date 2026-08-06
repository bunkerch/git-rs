# Pull

`Repository::pull` composes upload-pack fetching with fast-forward, merge, or
rebase integration. The repository and transport remain separate abstractions:
repository state uses its `FileSystem`, while callers provide any
`UploadPackTransport` implementation.

```rust
use git_rs::{
    PullMode, PullOptions, Repository, RepositoryTransport, Signature,
    UploadPackOptions,
};

fn pull_from_memory(
    local: &Repository,
    remote: &Repository,
    committer: &Signature,
) -> git_rs::Result<git_rs::PullResult> {
    let mut transport = RepositoryTransport::new(remote, UploadPackOptions::default());
    local.pull(
        &mut transport,
        &PullOptions {
            mode: PullMode::Rebase,
            ..PullOptions::default()
        },
        committer,
    )
}
```

The current branch is read from symbolic `HEAD`. By default, remote and source
come from `branch.<name>.remote` and `branch.<name>.merge`; `remote_name` and
`remote_branch` provide explicit overrides. The selected source must map to
exactly one local destination through the remote's positive fetch refspecs.
After fetch, the advertised source ID must equal that tracking ref, preventing a
stale ref from being integrated when the requested branch was not advertised.

`PullMode::FastForwardOnly` is the default and rejects divergence.
`PullMode::Merge` delegates to the configured `MergeOptions`, including
three-way conflicts and resumable merge state. `PullMode::Rebase` captures the
old tracking tip before fetch and uses it as the local-commit boundary, then
rebases onto the new advertised tip. If no old tracking ref exists, exactly one
merge base is required. A remote already contained in local history returns
up-to-date without rewriting local commits.

An unborn clean branch is initialized directly from its one selected upstream.
Detached HEAD is rejected because there is no branch upstream configuration to
update. Fetch happens before integration checks, so a rejected merge or rebase
still leaves valid downloaded objects and tracking refs, matching Git's useful
fetch-first behavior.

The operation writes `.git/FETCH_HEAD` in Git's single-merge-candidate format:

```text
<object-id>\t\tbranch '<name>' of <remote>
```

All protocol, pack, object, graph, diff, checkout, and rebase limits remain
available through nested `FetchOptions`, `MergeOptions`, and `RebaseOptions`.
The caller supplies the committer identity; no host clock or environment is
consulted.

## Git source comparison

The ordering follows `cmd_pull()` in `builtin/pull.c`: resolve policy and
original tip, capture the rebase fork point, fetch first, identify merge heads,
fast-forward an unborn branch, then choose ff-only, merge, or rebase. The Rust
API makes reconciliation explicit and returns typed fetch/integration results
instead of parsing command-line configuration or launching subordinate Git
commands.
