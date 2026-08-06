# Upload-pack

The library exposes Git protocol v0/v1 upload-pack as transport-neutral byte
operations. `Repository::advertise_upload_pack` produces the pkt-line ref
advertisement. `UploadPackRequest::parse` validates a complete stateless request
body, and `Repository::respond_upload_pack` returns the negotiation reply and,
after `done`, a pack stream.

No Git executable, socket, or HTTP implementation is involved. A caller can
connect these methods to an HTTP request body, SSH channel, TCP stream, or an
in-memory test harness.

```rust
# use git_rs::{InitOptions, MemoryFileSystem, Repository};
let repository = Repository::init(
    MemoryFileSystem::new(),
    "repo",
    &InitOptions::default(),
)?;
let advertisement = repository.advertise_upload_pack()?;
assert!(advertisement.ends_with(b"0000"));
# Ok::<(), git_rs::Error>(())
```

## Advertisement

`HEAD` is emitted first when it resolves, followed by every loose or packed ref
below `refs/` in bytewise order. Loose refs override packed duplicates. A
symbolic HEAD adds `symref=HEAD:<target>`. An unborn repository advertises the
zero-ID `capabilities^{}` pseudo-ref, matching Git.

The server advertises only behavior implemented by the response engine:

- `side-band-64k` for channel-1 pack framing;
- `ofs-delta` for compact pack deltas;
- `no-progress`, since the library does not mix progress into data;
- `object-format=sha1` and an `agent` identifier.

## Negotiation and reachability

The parser accepts one or more `want` lines, a flush, `have` lines, negotiation
flushes, and `done`. Capabilities are allowed only on the first want. Wants must
name advertised tips; arbitrary object-ID extraction is rejected.

A request without `done` receives an ACK or NAK negotiation pkt-line only. A
request with `done` additionally receives a pack. The server traverses commits,
parents, trees, blobs, and annotated-tag targets, excludes gitlink commits, and
subtracts the complete closure of locally known `have` objects. This makes the
pack complete without relying on thin-pack assumptions. Missing client haves
are ignored.

When `side-band-64k` is selected, pack chunks use band 1 and end in a flush.
Otherwise the raw `PACK` stream immediately follows the ACK/NAK pkt-line.
Object reads and pack construction retain caller-configured size bounds.

The `upload_pack` example demonstrates stateless request handling:

```text
cargo run --example upload_pack -- repository --advertise
cargo run --example upload_pack -- repository < request.pkt > response.bin
```

## Git source comparison

The implementation was compared directly with:

- `upload-pack.c:write_v0_ref` and `send_ref` for first-line capabilities,
  symbolic HEAD, and ref ordering;
- `upload-pack.c:receive_needs` for want syntax, first-want capabilities, and
  advertised-tip restrictions;
- `upload-pack.c:get_common_commits` for have/done ACK and NAK behavior;
- `list-objects.c` and `revision.c` for commit/tree reachability boundaries;
- `upload-pack.c:create_pack_file` for raw versus sideband pack delivery.
