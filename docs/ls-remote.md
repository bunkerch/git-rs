# Remote reference discovery

The ls_remote and ls_remote_v2 functions query an UploadPackTransport without
fetching or storing any object. They return structured LsRemoteEntry values and
the same tab-separated byte representation produced by native git ls-remote.
No local repository or host filesystem is required.

~~~rust
use git_rs::{LsRemoteOptions, RepositoryTransport, UploadPackOptions, ls_remote};

let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
let refs = ls_remote(&mut transport, &LsRemoteOptions::default())?;
for entry in refs.entries() {
    println!("{} {}", entry.id(), entry.name());
}
# Ok::<(), git_rs::Error>(())
~~~

## Selection

LsRemoteSelection supports all refs, branches, tags, or both branch and tag
namespaces. refs_only removes HEAD and peeled tag companions. show_symrefs adds
the symbolic target record for HEAD. Patterns use Git wildmatch rules against
the full ref or any tail beginning after a slash, so main matches
refs/heads/main but not refs/heads/domain.

Results sort by refname. Annotated tags include both refs/tags/name and
refs/tags/name^{} unless refs_only is set. Protocol-v0/v1 and protocol-v2
queries produce the same structured result.

## Protocol v2

ls_remote_v2 performs capability discovery and an ls-refs command with symrefs,
peel, and unborn arguments. Branch/tag selection is also sent as ref-prefix
arguments so a remote can avoid enumerating irrelevant namespaces. Server
options are validated against NUL and LF and are sent only when the server
advertises support.

## Bounds and validation

Options bound advertisement/response bytes, decoded ref count, and formatted
output bytes. Duplicate refs, invalid IDs, malformed packet framing, invalid
symref targets, unsupported object formats, and trailing protocol data are
rejected. The query never mutates either repository.

The implementation follows builtin/ls-remote.c and
Documentation/git-ls-remote.adoc in upstream Git. Tests compare v0 and v2
results including HEAD symrefs and peeled tags, and development checks compare
formatted output directly with native git ls-remote.

The host adapter example can query either protocol implementation:

~~~text
cargo run --example ls_remote -- REPOSITORY --symref
cargo run --example ls_remote -- REPOSITORY --v2 --tags "v*"
~~~
