# Fetch and clone

`git-rs` separates Git's upload-pack byte protocol from the network or process
that carries it. Implement `UploadPackTransport` for HTTP, SSH, a message queue,
or an in-process service. The library never invokes a command-line program and
the destination repository continues to use its configured `FileSystem`.

`RepositoryTransport` connects directly to another `Repository`, which makes
fully in-memory clones and deterministic tests possible:

```rust
use git_rs::{
    CloneOptions, InitOptions, MemoryFileSystem, Repository, RepositoryTransport,
    UploadPackOptions,
};

let source = Repository::init(
    MemoryFileSystem::new(),
    "source",
    &InitOptions::default(),
)?;
let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
let (clone, result) = Repository::clone_from(
    MemoryFileSystem::new(),
    "clone",
    &mut transport,
    &CloneOptions {
        remote_url: "memory://source".into(),
        ..CloneOptions::default()
    },
)?;
# let _ = (clone, result);
# Ok::<(), git_rs::Error>(())
```

Clone parses the protocol-v0/v1 advertisement, discovers the default branch,
requests all branch and tag tips, validates the received pack before publishing
it, writes remote configuration, and checks out the default branch for a
non-bare repository. A bare clone maps remote branches directly to
`refs/heads/*`, matching Git's bare-clone ref layout.

`Repository::fetch` uses all locally reachable ref tips as `have` lines and
updates `refs/remotes/<remote>/*`. Tags are fetched into `refs/tags/*`; an
existing tag is never silently moved. Pack-size, per-object, and aggregate
inflation limits are available in both `FetchOptions` and `CloneOptions`.

Setting `depth` requests a shallow history. The client advertises its existing
boundaries on later fetches, validates the server's shallow/unshallow update,
publishes the pack, and atomically writes the resulting `.git/shallow` set.
Increasing an absolute depth transfers only the newly exposed closure; reaching
the root removes the shallow file. `max_shallow_commits` bounds persisted and
wire-provided boundary state.

Ref changes are applied as one reference transaction after pack validation and
publication. A transport failure, malformed advertisement, corrupt pack, or ref
conflict therefore cannot expose refs that point at unavailable objects.

## Git source comparisons

The implementation's behavioral comparisons are based on:

- `connect.c` for v0/v1 advertisement and capability parsing;
- `fetch-pack.c` for `want`, `have`, `done`, ACK/NAK, shallow updates, and
  sideband negotiation;
- `builtin/fetch.c` and `remote.c` for fetch mappings and tag update safety;
- `builtin/clone.c` for default-branch selection, bare ref mapping, remote
  configuration, and initial checkout.

The Rust implementation is independently structured around safe byte slices,
owned protocol values, transactional refs, and the abstract filesystem API; no
Git or gitoxide code is linked or copied.

For a host-backed, in-process demonstration, run:

```console
cargo run --example clone_local -- path/to/source path/to/destination
cargo run --example clone_local -- path/to/source path/to/shallow-clone 1
cargo run --example fetch_local -- path/to/source path/to/shallow-clone 2
```
