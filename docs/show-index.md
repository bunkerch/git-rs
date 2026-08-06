# Inspecting pack indexes

The pure `show_index` function parses a complete pack index byte slice without
opening its sibling pack. `Repository::show_index` reads the same format from a
path through the configured filesystem adapter.

```rust
use git_rs::{ShowIndexOptions, show_index};
# fn example(index_bytes: &[u8]) -> git_rs::Result<()> {
let report = show_index(index_bytes, &ShowIndexOptions::default())?;
for entry in report.entries {
    println!("{} {} {:?}", entry.offset, entry.id, entry.crc32);
}
# Ok(())
# }
```

SHA-1 index versions 1 and 2 are supported. Version 1 contains interleaved
32-bit offsets and object IDs and therefore reports no CRC. Version 2 reports
CRC32 values and resolves its indexed 64-bit offset table. Entries remain in
the index's object-ID order.

Before returning data, both decoders validate the index checksum, monotonic and
exact fanout table, strict object-ID ordering, unique offsets, exact/truncated
layout, and supported version. Version 2 additionally validates large-offset
references. Input bytes and object counts are bounded before result allocation.
The report also exposes the corresponding pack checksum.

This implementation follows `builtin/show-index.c` and
`Documentation/git-show-index.adoc`. It is independent of pack-body access and
does not invoke Git. The example reproduces Git's textual output from stdin:

```text
cargo run --example show_index <objects.idx
```
