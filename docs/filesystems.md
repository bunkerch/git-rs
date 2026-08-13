# Filesystem adapters

Implement `FileSystem` to store a repository anywhere. The trait works with
complete byte buffers intentionally. Loose objects, refs, indexes, config, and
packfiles all follow the same backend-neutral contract. Adapters remain free to
cache or otherwise optimize how those complete values are obtained.

All paths passed to an adapter are relative to its storage root. Adapters must
reject absolute paths and parent traversal. Directory reads return child names,
not recursively expanded paths.

`write_new` is lock acquisition and must fail if the path exists. `publish(from,
to)` is the file-publication boundary: readers of the destination must see its
complete old or complete new value without an availability gap. Host adapters
normally implement this as a rename. Object stores can upload immutable bytes,
atomically replace the destination key or pointer, and then clean up the source;
an interrupted cleanup may temporarily leave both names without making the
destination unavailable.

`rename(from, to)` retains the stronger contract needed for worktree moves: it
atomically relocates a file, symlink, or complete directory subtree. Remote
adapters can implement directory moves with a database transaction, namespace
prefix swap, object generation plus compare-and-swap, or another
storage-native atomic primitive. Bare repository ref and pack publication uses
`publish`, so it does not require an atomic object-store directory rename.

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
