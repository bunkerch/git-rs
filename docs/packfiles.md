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

## Git source comparison

The implementation was compared directly with:

- `pack.h:PACK_IDX_SIGNATURE` and `packfile.c:load_idx` for index layout and
  validation;
- `packfile.c:unpack_object_header_buffer` for packed object headers;
- `packfile.c:unpack_delta_entry` for delta base handling and depth behavior;
- `patch-delta.c:patch_delta` for delta varints, copy/insert opcodes, the special
  64 KiB copy size, and output bounds.

Tests include constructed packs stored entirely in memory plus checksum and
delta corruption cases. Interoperability was also validated against direct and
delta-compressed entries in packs produced by Git.
