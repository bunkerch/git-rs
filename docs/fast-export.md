# Fast-export

Repository::fast_export creates a Git fast-import stream using only repository
and FileSystem APIs. It does not invoke a process and works with host, memory,
or user-defined storage adapters.

The caller supplies fully qualified ReferenceName values. All commits reachable
from those refs are emitted oldest-first, each as a full tree. Blobs are
deduplicated by object ID, merge parents retain their order, annotated tags are
preserved, and final reset commands reproduce every selected branch or
lightweight tag. A temporary stream ref is deleted before completion.

~~~rust
use git_rs::{FastExportOptions, ReferenceName};

let refs = [ReferenceName::branch("main")?];
let export = repository.fast_export(&refs, &FastExportOptions::default())?;
send_to_importer(export.stream());
# Ok::<(), git_rs::Error>(())
~~~

## Marks and incremental export

FastExportResult::marks maps object IDs to numeric marks. Pass that map back
through FastExportOptions::initial_marks on a later call to omit history already
accepted by the destination. This replaces native Git's host-path-based
import-marks and export-marks files with values that applications can persist
through any backend.

The corresponding importer takes the inverse map through
FastImportOptions::initial_marks. A service can therefore store marks in a
database while repository objects remain in memory, object storage, or a
hybrid adapter.

## Fidelity and policy

Full-tree emission avoids heuristic rename/copy detection and makes output
deterministic. It is larger than parent-diff output but still emits each blob
only once. Paths use Git C-style byte quoting, so non-UTF-8 names are lossless.
Commit author, committer, message, encoding, signatures, parents, modes, and
trees are preserved. Annotated-tag targets, taggers, messages, and signatures
inside the message are preserved.

Unsupported extra object headers cause an error instead of being discarded.
Tree-targeting annotated tags cannot be represented by the fast-import grammar
and are rejected. Annotated tag refs must agree with the tag's embedded name so
that exporting cannot silently change the object ID.

FastExportOptions bounds graph traversal, object count, object reads, and total
output bytes. Existing marks are validated for uniqueness and object
availability before traversal.

## Compatibility verification

The implementation follows Documentation/git-fast-export.adoc and
builtin/fast-export.c from upstream Git. Tests round-trip branches, merge
commits, executable modes, binary blobs, non-UTF-8 paths, and annotated tags
through git-rs fast-export and fast-import while requiring identical object IDs.
Development compatibility checks also feed generated streams into native Git
fast-import and validate the resulting repository with strict fsck.

The host-filesystem example writes a stream without making Git CLI calls:

~~~text
cargo run --example fast_export -- REPOSITORY OUTPUT refs/heads/main refs/tags/v1
~~~
