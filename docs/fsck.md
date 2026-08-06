# Repository integrity checking

`Repository::fsck` validates the complete object database through the configured
`FileSystem`. It performs no host-specific access and does not invoke Git.

The verifier enumerates loose objects and version-2 pack indexes, then reads
every distinct object through the normal checked object path. This verifies
loose-object hashes and compression as well as pack/index checksums, entry
boundaries, deltas, reconstructed hashes, and configured size limits. Commit,
tree, and annotated-tag bodies are parsed and every non-gitlink edge must name
an object of the required type.

```rust
use git_rs::{FsckOptions, Repository};

fn verify(repository: &Repository) -> git_rs::Result<()> {
    let report = repository.fsck(&FsckOptions::default())?;
    println!("checked {} objects", report.objects);
    for id in report.dangling() {
        println!("dangling {id}");
    }
    Ok(())
}
```

Refs and `HEAD` are retention roots. By default reflog old/new IDs and non-gitlink
index entries are roots too, which protects recent history and staged blobs.
`additional_roots` lets a server or transaction retain objects not yet named by
a repository ref. Gitlink targets are intentionally not required: their commits
belong to the submodule's object database.

Unreachable objects are valid objects outside the retained graph, so they are
reported rather than rejected. A dangling object is an unreachable object not
referenced by another inspected object. `max_objects` and `max_object_size`
bound work and memory for repositories backed by remote or untrusted storage.

Run the host-backed example with:

```console
cargo run --example fsck -- /path/to/repository
```

## Git source comparison

The implementation follows the separation in `builtin/fsck.c` between object
presence (`HAS_OBJ`), reachability (`REACHABLE`), and inbound use (`USED`). Its
typed link checks correspond to the `fsck_walk_options` traversal and broken-link
reporting there. Object-format validation is delegated to git-rs's typed parsers
and pack reader instead of copying Git's callback-oriented C implementation.
As in Git's default full check, refs, reflogs, and index objects contribute
retention roots; replace objects, commit graphs, multi-pack indexes, alternates,
and promisor-object exemptions are not repository formats currently supported
by this library and therefore cannot silently weaken verification.
