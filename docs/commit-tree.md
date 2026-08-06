# Creating commit objects directly

`Repository::commit_tree` creates a commit object without changing `HEAD` or
any reference. The tree, parent list, author, committer, and message sources are
explicit, making the operation deterministic and usable with every filesystem
adapter.

~~~rust
use git_rs::{CommitTreeMessagePart, CommitTreeOptions};

let id = repository.commit_tree(
    tree_id,
    &[first_parent, second_parent],
    &[
        CommitTreeMessagePart::Paragraph(b"subject".to_vec()),
        CommitTreeMessagePart::FileContents(b"body\n".to_vec()),
    ],
    &author,
    &committer,
    &CommitTreeOptions::default(),
)?;
# let _ = id;
# Ok::<(), git_rs::Error>(())
~~~

The selected tree must exist as a tree object and every distinct parent must
exist as a commit. Duplicate parents are ignored after their first occurrence,
preserving first-seen order. Parent count, message bytes, and decoded validation
objects have independent limits.

`Paragraph` matches repeated `git commit-tree -m`: each value is LF-completed
and separated from prior non-empty content by another LF. `FileContents`
matches `-F` or standard input and appends exact bytes after the same separator.
NUL is rejected. Without an explicit non-UTF-8 encoding, invalid UTF-8 bytes
are repaired as Latin-1 exactly as Git does. A non-UTF-8 encoding preserves the
message and adds the canonical `encoding` header; `UTF-8` and `UTF8` aliases do
not add one.

Identities are explicit `Signature` values rather than ambient process
environment, so callers control timestamps and timezone offsets and tests are
reproducible. Applications can resolve identity configuration before calling
the plumbing API. `commit_tree_signed` accepts a `CommitSigner` implementation,
passes it the exact unsigned body, and inserts its result as a canonical
multiline `gpgsig` header. This supports GPG, SSH, or application-managed keys
without invoking a process or adding a cryptographic dependency to the crate.

The behavior follows `builtin/commit-tree.c`, `commit.c:write_commit_tree`, and
`commit.c:ensure_utf8` from upstream Git. Tests cover parent ordering and
deduplication, message composition, type checks, bounds, NUL rejection,
Latin-1 repair, encoding headers, and signing payload/header formatting. Native
checks compare complete commit bytes and object IDs under fixed identities.

~~~text
printf 'message\n' | cargo run --example commit_tree -- \
  REPOSITORY TREE 'A U Thor' author@example.com 1700000000 +0130 [PARENT...]
~~~
