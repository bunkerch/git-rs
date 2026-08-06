# Upload-pack

The library exposes Git protocol v0/v1 and v2 upload-pack as transport-neutral
byte operations. `Repository::advertise_upload_pack` and
`Repository::advertise_upload_pack_v2` produce the corresponding capability
advertisements. `UploadPackRequest` and `UploadPackV2Request` validate complete
stateless requests before repository access. The response APIs return
negotiation replies and pack streams without owning a socket or process.

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
Object graph walks are additionally bounded by `UploadPackOptions::max_objects`.

## Protocol v2

The v2 capability advertisement contains only implemented behavior:
`agent`, `object-format=sha1`, `ls-refs=unborn`, and `fetch`. `ls-refs` supports
`symrefs`, annotated-tag `peel`, repeated literal `ref-prefix` filters, and
unborn symbolic HEAD. Its request bytes, capability count, argument count,
prefix count, reference count, tag depth, object count, and individual object
size are bounded.

The base v2 `fetch` command supports `want`, `have`, `done`, `thin-pack`,
`no-progress`, `include-tag`, `ofs-delta`, `shallow`, absolute `deepen`, and
`deepen-relative`.
A complete pack is valid when a
client permits a thin pack, so `thin-pack` is accepted without creating an
external-base delta. Negotiation responses use the `acknowledgments` section;
completed requests use the `packfile` section and mandatory sideband framing.
Depth fetches use a `shallow-info` section and compute boundary commits with a
bounded breadth-first history walk. Relative deepening expands every reached
client boundary. Time/revision cutoffs and partial-clone filters remain
unadvertised and are rejected.

The `upload_pack` example demonstrates stateless request handling:

```text
cargo run --example upload_pack -- repository --advertise
cargo run --example upload_pack -- repository < request.pkt > response.bin
cargo run --example upload_pack -- repository --v2-advertise
cargo run --example upload_pack -- repository --v2 < request.pkt > response.bin
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
- `Documentation/gitprotocol-v2.adoc`, `serve.c`, `ls-refs.c`, and
  `upload-pack.c:upload_pack_v2` for v2 capabilities, command framing,
  reference attributes, response sections, and fetch argument semantics.
