# Email patch series

`Repository::format_patches` turns a bounded revision range into an in-memory
series of mail messages. It does not invoke Git or write output files, and all
repository reads use the configured filesystem adapter.

```rust
use git_rs::{FormatPatchOptions, ObjectId, Repository};

fn patches(
    repository: &Repository,
    tip: ObjectId,
    upstream: ObjectId,
) -> git_rs::Result<Vec<git_rs::FormatPatch>> {
    repository.format_patches(&[tip], &[upstream], &FormatPatchOptions::default())
}
```

Each `FormatPatch` exposes its commit ID, a deterministic suggested filename,
and complete mail bytes. The messages contain a Git `From` separator, author
identity and RFC 2822 date, `[PATCH]` subject, commit-message body, separator,
unified diff, and optional `-- ` signature. Non-ASCII subjects and author names
use RFC 2047 Base64 encoding.

The revision set is topologically reversed into oldest-first application order.
Merge commits are omitted because representing a merge as a single first-parent
diff loses its topology. `RevisionWalkOptions::max_count` limits output after
that omission. `Auto` numbering numbers series with more than one patch;
`Always` and `Never` make the policy explicit. A reroll count produces both
`[PATCH vN i/M]` subjects and `vN-` filenames.

`FormatPatchOptions` bounds graph traversal, object and diff work, commit
message and subject sizes, each rendered patch, aggregate mail bytes, and
filename length. Output stays in memory so callers can persist it on the host,
an object store, a database, or send it over their own transport.

## Git source comparison

The ordering and series policy follow `cmd_format_patch()` in `builtin/log.c`:
commits are selected by revision traversal, emitted in reverse order, numbered
according to series policy, and written with a configurable subject prefix,
reroll count, and signature. Patch bodies use the same parent-tree comparison
that Git routes through `log_tree_diff()`. The library API returns typed values
instead of implementing command-line output directories, cover-letter files,
or transport concerns.
