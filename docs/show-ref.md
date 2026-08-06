# Showing references

`Repository::show_refs` returns a resolved, ordered view of loose and packed
references. Loose values override packed values, symbolic references are
resolved with Git's hop limit, and every reported object is loaded and
validated rather than allowing a broken reference through.

```rust
# use git_rs::{Repository, ShowRefOptions};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let references = repository.show_refs(&ShowRefOptions {
    include_head: true,
    dereference_tags: true,
    ..ShowRefOptions::default()
})?;
for reference in references {
    println!("{} {}", reference.id(), reference.name());
}
# Ok(())
# }
```

`branches_only` and `tags_only` select namespaces and may be combined. Patterns
match either a complete name or a slash-delimited suffix: `main` matches
`refs/heads/main` but not `refs/heads/domain`. Requested `HEAD` is always first,
matching `git show-ref --head`. With `dereference_tags`, each annotated tag is
immediately followed by a `refs/tags/name^{}` entry for its final peeled object.

Reference count, directory depth, tag depth, and object size are bounded.
Prefix-only enumeration prunes unrelated loose-reference directories. All
storage access uses the repository filesystem adapter and no dependency or
executable is added.

## Git source comparisons

- `builtin/show-ref.c:show_ref` defines component-suffix filtering and `HEAD`
  behavior.
- `builtin/show-ref.c:show_one` defines object validation and peeled-tag output.
- `refs/files-backend.c` defines loose-over-packed precedence.
- `refs/packed-backend.c` defines packed-reference parsing and ordering.

The implementation is independently structured safe Rust.
