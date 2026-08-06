# Three-way file merging

`merge_file` combines changes from `current` and `other` relative to `base`
without reading or writing a filesystem. `Repository::merge_blobs` provides the
same operation for stored blob IDs and can publish the result as a new blob
through any `FileSystem` adapter.

```rust
use git_rs::{merge_file, MergeFileOptions, MergeFileStyle};

let result = merge_file(
    b"one\ncurrent\n",
    b"one\nbase\n",
    b"one\nother\n",
    &MergeFileOptions {
        style: MergeFileStyle::Diff3,
        current_label: b"ours".to_vec(),
        base_label: b"ancestor".to_vec(),
        other_label: b"theirs".to_vec(),
        ..MergeFileOptions::default()
    },
)?;
println!("{} conflicts", result.conflicts());
# Ok::<(), git_rs::Error>(())
```

Normal, diff3, and zealous-diff3 conflict styles are available. Marker lengths
and all three byte-valued labels are configurable. `MergeFileFavor::Ours`,
`Theirs`, and `Union` resolve overlapping regions without markers; the default
returns markers and an exact conflict count. Common lines are refined out of
normal and zdiff3 conflicts, and CRLF inputs receive CRLF marker lines.

Like `git merge-file`, NUL-bearing binary inputs are rejected. Input bytes,
output bytes, line counts, and Myers trace cells have independent bounds.
Marker sizes and labels are checked against the output bound before allocation.
The implementation is iterative over edit regions.

For object-store operation, set `write_object` in `MergeFileOptions`; the result
then includes `object_id()` for the newly stored blob. With it disabled, blob
inputs are read through abstract storage but no repository state is changed.

## Git source comparison

The public behavior is compared with `/home/coder/git/builtin/merge-file.c`.
Conflict grouping, favor modes, marker rendering, and zealous refinement follow
the documented behavior of `/home/coder/git/xdiff/xmerge.c`, while using the
crate's independently implemented bounded Myers edit engine. The shared core is
also used by tree merges and replay operations, preventing standalone and
repository merges from diverging.

Run the byte-file example with:

```sh
cargo run --example merge_file -- current base other --zdiff3 --marker-size=9
```
