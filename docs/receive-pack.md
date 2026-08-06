# Receive-pack

The receive-pack API accepts smart-protocol v0/v1 pushes without invoking Git
or coupling the library to a network transport:

- `Repository::advertise_receive_pack` emits refs and capabilities;
- `ReceivePackRequest::parse` separates command pkt-lines from the raw pack;
- `Repository::receive_pack` validates the pack in quarantine, checks object
  connectivity and compare-and-swap preconditions, publishes accepted objects,
  updates refs, and produces report-status pkt-lines.

## Incoming pack quarantine

`Repository::validate_incoming_pack` is independently usable for object
ingestion. It verifies the pack signature, version, declared object count,
trailer SHA-1, exact end of every zlib stream, object sizes, OFS and REF delta
bases, delta instructions, reconstructed IDs, and duplicate IDs before anything
is written.

`IncomingPackOptions` independently bounds compressed pack bytes, each inflated
object or delta program, and the aggregate inflated size. Receive-pack exposes
the same limits through `ReceivePackOptions`, preventing object-count and
decompression inputs from turning attacker-controlled headers into unbounded
allocations.

REF deltas can use an existing repository object, which accepts thin push packs.
Validated incoming objects are rebuilt into a self-contained pack and v2 index;
external thin bases are therefore not required after publication.
`ValidatedPack::object_ids` and `contains` permit policy checks while the pack is
still quarantined. `Repository::publish_validated_pack` performs the explicit
publication step.

## Commands and connectivity

Each command is parsed as `<old-id> <new-id> <refname>`. Ref names use the same
strict grammar as local ref operations, command names must be unique, and the
old ID is enforced with compare-and-swap during both validation and final
update. A non-bare repository refuses updates to its checked-out symbolic HEAD
branch by default.

Before publication, each new tip is walked through commits, parent links,
trees, blobs, and annotated tags using both quarantined and existing objects.
Gitlinks are boundaries. A missing object rejects only the affected command.
Successful commands can proceed independently, matching non-atomic
receive-pack behavior.

The current advertisement deliberately contains only implemented capabilities:
`report-status`, `delete-refs`, `atomic`, `ofs-delta`, `object-format=sha1`, and
`agent`. Push-options, signed-push, and sideband capabilities are not advertised
or silently accepted. Deletion locks both the loose path and
`packed-refs`, removes a hidden packed copy so it cannot reappear, and removes
the corresponding reflog.

With `atomic`, any policy, connectivity, or stale-value failure marks every
command failed. Accepted commands are passed to one batch ref transaction,
which locks all names in deterministic order, validates every old value while
all locks are held, prepares packed deletions, and then publishes the batch.

## Example

The stateless example reads a complete request from standard input:

```text
cargo run --example receive_pack -- server.git --advertise
cargo run --example receive_pack -- server.git < request.bin > status.pkt
```

All object and ref access passes through `FileSystem`, so the same operation
works with host, memory, or custom hybrid adapters.

## Git source comparison

The implementation was compared directly with:

- `builtin/receive-pack.c:show_ref` and `write_head_info` for advertisement;
- `read_head_info` and `queue_command` for command and capability framing;
- `unpack` and `unpack_with_sideband` for incoming pack validation boundaries;
- `execute_commands` for quarantine-before-ref visibility and connectivity;
- `execute_commands_non_atomic` for independent command results;
- `report` for `unpack ok`, `ok`, `ng`, and flush response pkt-lines;
- `builtin/index-pack.c` and `builtin/unpack-objects.c` for delta-base and
  checksum handling.
