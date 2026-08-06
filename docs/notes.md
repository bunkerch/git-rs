# Notes

Git notes attach arbitrary blob content to an existing object without rewriting
that object. `git-rs` reads and writes the standard `refs/notes/*` commit and
tree representation, so notes created through an abstract filesystem remain
compatible with Git when that storage is materialized as a repository.

```rust
# use git_rs::{NotesOptions, Repository, Signature};
# fn example(repository: &Repository, commit: git_rs::ObjectId) -> git_rs::Result<()> {
let identity = Signature::new("Notes Bot", "notes@example.com", 1, 0)?;
let options = NotesOptions::default();

repository.add_note(
    commit,
    b"reviewed\n",
    false,
    &identity,
    &identity,
    &options,
)?;
assert_eq!(repository.note(commit, &options)?.unwrap().message(), b"reviewed\n");
# Ok(())
# }
```

`NotesOptions::reference` defaults to `refs/notes/commits` and must remain
inside `refs/notes/`. Its object-size, leaf-count, and tree-depth limits apply
before untrusted notes are returned. `list_notes` accepts flat object-ID names
and arbitrary fanout splits whose path components concatenate to a SHA-1 ID.

`add_note` requires `force` to replace an existing note. `copy_note` reuses the
immutable source blob and applies the same replacement rule. `remove_note`
fails if the note is absent. Every mutation preserves non-note tree entries,
creates a notes commit parented by the former ref tip, and compare-and-swap
updates the ref and reflog. A concurrent writer therefore fails rather than
silently losing notes.

## Git source comparisons

- `notes.c:add_note`, `remove_note`, and `get_note` define target-to-blob
  behavior.
- `notes.c:write_notes_tree` defines fanout tree writing and preservation of
  non-note entries.
- `notes-utils.c:create_notes_commit` and `commit_notes` define notes commit
  parenting and ref updates.
- `builtin/notes.c` defines add, copy, remove, force, and `refs/notes/`
  constraints.

The implementation is independently structured safe Rust and operates only
through the repository's `FileSystem` adapter. It invokes neither Git nor any
native Git library.
