# Commit graphs

`git-rs` reads and writes Git's single-file commit-graph version 1 at
`objects/info/commit-graph`. The implementation uses only the repository's
`FileSystem`, so the same API works with host, memory, object-store, database,
or hybrid adapters.

```rust
use git_rs::{CommitGraphOptions, InitOptions, MemoryFileSystem, Repository};

let repo = Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default())?;
let tips = []; // IDs of commits whose complete ancestry should be indexed.
let report = repo.write_commit_graph(&tips, &CommitGraphOptions::default())?;
let graph = repo.read_commit_graph(report.bytes + 1, 1_000_000)?;
assert!(graph.is_empty());
# Ok::<(), git_rs::Error>(())
```

The writer closes the supplied tips over all parents, reads objects without
applying replacement refs, sorts IDs bytewise, computes saturated topological
levels, writes `OIDF`, `OIDL`, and `CDAT`, and adds `EDGE` for octopus merges.
It publishes with the normal lock-and-rename path and avoids rewriting identical
bytes unless `force` is set. `dry_run` performs all object reads and encoding but
does not publish.

The parser verifies the SHA-1 trailer before trusting chunk contents. It also
checks the header, chunk table and offsets, required chunk sizes, monotonic
fanout, strict OID ordering, parent positions, terminated extra-edge lists,
generation numbers, and caller limits. Unknown optional chunks are skipped as
the chunk format intends. Split graph chains, SHA-256 repositories, generation
data v2, and changed-path Bloom filters are not accepted by this single-file API.

`Repository::is_ancestor` uses a valid graph when both endpoints are present,
pruning traversal with topological levels. It falls back to commit objects when
the file or either endpoint is absent. As Git does, it disables this acceleration
when replacement refs are active because replacements can change parent edges.

On a host repository, index the commit reachable from `HEAD` with:

```console
cargo run --example commit_graph -- /path/to/repository
git -C /path/to/repository commit-graph verify
```

The second command is an interoperability check only; the library and example
do not invoke Git.

## Source correspondence

The format and validation rules correspond to Git's
`Documentation/gitformat-commit-graph.adoc`, `commit-graph.h`, and the parsing,
generation, and chunk-writing paths in `commit-graph.c`. The Rust code is an
independent implementation: it uses those files as behavioral specifications
and does not translate or copy gitoxide code.
