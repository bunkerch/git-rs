# Push client

`Repository::push` is the transport-neutral counterpart to the receive-pack
server. It obtains a protocol-v0/v1 advertisement, builds compare-and-swap ref
commands from the advertised old IDs, computes the local object closure, omits
objects already reachable from advertised remote refs, writes a Git pack, and
strictly parses the server's `report-status` response.

The `ReceivePackTransport` trait owns only byte exchange. Implementations can
carry those bytes over HTTP, SSH, an application RPC, or any other channel.
`InProcessReceivePackTransport` connects two `Repository` values directly, so
both ends may use memory, host, S3, database, or hybrid filesystem adapters.

```rust
use git_rs::{
    InProcessReceivePackTransport, PushOptions, PushUpdate, ReceivePackOptions,
    ReferenceName,
};
# use git_rs::Repository;
# fn example(local: &Repository, remote: &Repository) -> git_rs::Result<()> {
let tip = local.resolve_reference("refs/heads/main")?;
let mut transport = InProcessReceivePackTransport::new(
    remote,
    ReceivePackOptions::default(),
);
let result = local.push(
    &mut transport,
    &[PushUpdate::update(ReferenceName::branch("main")?, tip)],
    &PushOptions::default(),
)?;
assert!(result.is_ok());
# Ok(())
# }
```

Ordinary branch updates must be fast-forwards. Use
`PushUpdate::force_update` when replacement is intentional. Existing tags also
require force. `PushUpdate::delete` requests deletion only when the remote
advertises `delete-refs`. `PushOptions::atomic` requires the server's `atomic`
capability and makes all commands one remote transaction.

Remote ref rejection is represented in `PushResult::statuses`; malformed
advertisements, unsupported requested features, unpack failure, malformed
status, missing local objects, and transport failures return `Error`.

For a host-backed in-process demonstration:

```console
cargo run --example push_local -- source remote.git refs/heads/main refs/heads/main
```

## Git source comparisons

Behavior is compared with Git's `send-pack.c` for capability selection,
command framing, pack necessity, atomic handling, and report-status parsing;
`remote.c` for update classification; and `receive-pack.c` for command and
status semantics. In particular, `delete-refs` is checked as a server-advertised
capability and is not echoed as a client capability.

The implementation is independently structured in safe Rust around explicit
owned updates, bounded object reads, the existing pack writer, and abstract
repository storage. It neither invokes Git nor links or copies Git/gitoxide.
