# Packfiles

`Repository::read_object` first checks the loose object fanout and then searches
version-2 indexes in `objects/pack`. This works through `FileSystem`, including
`MemoryFileSystem`; packed reads never bypass the storage abstraction.

The reader supports pack versions 2 and 3, direct commit/tree/blob/tag entries,
and OFS_DELTA and REF_DELTA chains whose bases are in the same pack. Delta depth
is capped at 64 and every allocation is bounded by the caller's `max_size`.

Before returning an object, the implementation validates:

- the index header, version, exact table layout, monotonic fanout, sorted object
  IDs, large-offset references, and trailing SHA-1;
- the pack signature, version, object count, pack SHA-1, and agreement with the
  checksum recorded by the index;
- the selected entry's CRC-32, object framing, inflated size, delta program,
  reconstructed size, and final Git object ID.

`PackIndex::parse`, `PackIndex::entries`, and `PackIndex::find` are public so a
storage adapter or higher-level object cache can validate and retain index data
without duplicating the format parser.

Repository handles cache validated indexes and immutable content-addressed pack
bytes across clones of the handle. Newly appearing index filenames are still
discovered on each lookup. Sorted offset tables make entry-boundary lookup
logarithmic instead of scanning every index entry; this is important during
reachability walks over an already packed repository.

## Creating packs

`Repository::build_pack` returns a `PackBundle` containing a complete pack and
index without assuming where either will be stored. `Repository::write_pack`
publishes the same bytes under Git's content-derived
`objects/pack/pack-<checksum>.{pack,idx}` names through `FileSystem`.

```rust
# use git_rs::{InitOptions, MemoryFileSystem, ObjectKind, PackOptions, Repository};
# let repository = Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default())?;
let first = repository.write_object(ObjectKind::Blob, b"first contents")?;
let second = repository.write_object(ObjectKind::Blob, b"second contents")?;
let written = repository.write_pack(&[first, second], &PackOptions::default())?;
assert_eq!(written.object_count, 2);
# Ok::<(), git_rs::Error>(())
```

Input IDs are de-duplicated without changing their first-seen order. The writer
uses direct entries for every object type and evaluates depth-one OFS deltas
against an earlier direct object of the same type. A delta is selected only when
its complete encoded entry is smaller. This bounds reconstruction depth while
still compacting similar content. CRC-32 uses a compile-time lookup table, and
the index is sorted once by object ID.

Pack bytes are published before index bytes. Since readers discover packs by
their index, they cannot observe an index for a partially published pack.
Existing content-addressed files must match byte-for-byte.

Incoming pack quarantine, including thin REF-delta resolution and self-contained
repacking, is documented in [`receive-pack.md`](receive-pack.md).

The `pack_objects` example writes a pack stream to standard output:

```text
cargo run --example pack_objects -- my-repository <object-id>...
```

## Git source comparison

The implementation was compared directly with:

- `pack.h:PACK_IDX_SIGNATURE` and `packfile.c:load_idx` for index layout and
  validation;
- `pack-write.c:write_pack_header`, `write_idx_file`, and
  `encode_in_pack_object_header` for pack and index emission;
- `packfile.c:unpack_object_header_buffer` for packed object headers;
- `packfile.c:unpack_delta_entry` for delta base handling and depth behavior;
- `patch-delta.c:patch_delta` for delta varints, copy/insert opcodes, the special
  64 KiB copy size, and output bounds.

Tests include constructed packs stored entirely in memory plus checksum and
delta corruption cases. Interoperability was also validated against direct and
delta-compressed entries in packs produced by Git.
