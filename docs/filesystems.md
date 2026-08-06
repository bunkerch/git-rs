# Filesystem adapters

Implement `FileSystem` to store a repository anywhere. The trait works with
complete byte buffers intentionally: loose objects, refs, indexes, and config
files are bounded records, while pack streaming will use a separate random-access
object interface so large packs are never forced into one allocation.

All paths passed to an adapter are relative to its storage root. Adapters must
reject absolute paths and parent traversal. Directory reads return child names,
not recursively expanded paths.

`write_new` is lock acquisition and must fail if the path exists. `rename(from,
to)` is the publication boundary and must atomically replace a file at `to`.
Remote adapters can implement these with a database transaction, object
generation plus compare-and-swap, or another storage-native atomic primitive.

Metadata is no-follow: adapters distinguish regular files, directories, and
symbolic links. `read_link` returns target bytes, and executable state plus
`FileStat` fields support Git index semantics without exposing host-specific
metadata types.

Built-in adapters:

- `MemoryFileSystem`: cloneable, shared, lock-protected storage for tests and
  ephemeral repositories.
- `HostFileSystem`: ordinary directories and files compatible with the Git CLI.

## Custom routing adapter

A hybrid adapter can inspect the path prefix: route `refs`, `HEAD`, and
`packed-refs` to PostgreSQL, while routing `objects/pack` to S3. Git operations
only observe the `FileSystem` contract and do not need storage-specific branches.
