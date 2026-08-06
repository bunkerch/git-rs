# Fast-import

Repository::fast_import ingests Git fast-import streams directly into any
FileSystem implementation. It does not invoke Git, use host paths, or bypass
the repository object and reference APIs.

~~~rust
use git_rs::{FastImportOptions, InitOptions, MemoryFileSystem, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    ".",
    &InitOptions::default(),
)?;
let stream = b"blob\nmark :1\ndata 5\nhello\n\
commit refs/heads/main\nmark :2\n\
committer Example <example@example.com> 1 +0000\n\
data 7\ninitial\nM 100644 :1 README\ndone\n";
let result = repository.fast_import(stream, &FastImportOptions::default())?;
assert_eq!(repository.resolve_reference("refs/heads/main")?, result.marks()[&2]);
# Ok::<(), git_rs::Error>(())
~~~

## Supported stream commands

The importer supports blobs, commits, annotated tags, resets, aliases,
checkpoints, progress messages, done, get-mark, cat-blob, and both named and
active-commit forms of ls. Commit file commands include modify, delete, copy,
rename, delete-all, and notes. Data may use an exact byte count or a delimiter.
Paths may be unquoted or use Git's C-style quoting, including octal escapes.
Authors, committers, taggers, merges, encodings, original object IDs, and
SHA-1/SHA-256 signature headers are parsed.

The accepted feature declarations are done, force, notes, get-mark, cat-blob,
ls, and the raw date formats. Non-semantic tuning option commands are accepted.
Features that name host files, including import-marks and export-marks, are
deliberately rejected because they would violate the filesystem abstraction.

To continue an import without mark files, pass prior marks through
FastImportOptions::initial_marks; the complete map is returned by
FastImportResult::marks.

## Publication and failure behavior

Objects are immutable and may become unreachable if a later command fails.
Branch and tag updates remain buffered and are published as one reference
transaction only at a checkpoint or successful end of stream. Consequently, a
failed stream exposes no partial reference changes after its last successful
checkpoint. Updates must be fast-forwards unless force is enabled.

A checkpoint intentionally establishes a durable prefix: a later error does
not roll back reference changes published by an earlier checkpoint.

## Resource policy

FastImportOptions bounds stream size, command count, individual and total data
size, mark count, tracked paths, changes per commit, parent count, object
reads, graph traversal, and response buffering. Defaults are generous for
production imports, while callers accepting untrusted streams should choose
limits appropriate for their service.

## Compatibility

Objects use the normal loose/packed object APIs and refs use the normal
transaction API. On HostFileSystem, the resulting repository is therefore
readable by native Git. The implementation and tests were compared with
Documentation/git-fast-import.adoc and builtin/fast-import.c in the upstream
Git source tree. Compatibility tests import the same byte stream into native
Git and git-rs and compare response bytes, object IDs, trees, tag bytes, and
strict fsck results.

The command-line example accepts a repository and stream file:

~~~text
cargo run --example fast_import -- REPOSITORY STREAM_FILE --require-done
~~~

The example is only an adapter around the library API; the library itself
performs no process or CLI calls.
