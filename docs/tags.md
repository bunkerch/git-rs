# Tags

`AnnotatedTag` models Git's tag object: target ID and declared type, tag header
name, optional tagger identity, preserved extra headers, and arbitrary message
bytes. Signature blocks are message content and round-trip unchanged.

```rust
# use git_rs::{ObjectKind, Repository, Signature, TagBuilder};
# fn example(repository: &Repository, commit: git_rs::ObjectId) -> git_rs::Result<()> {
let tag = TagBuilder::new(
    commit,
    ObjectKind::Commit,
    "v1.0",
    Signature::new("Tagger", "tagger@example.com", 1, 0)?,
)?
.message(b"release 1.0\n".to_vec())
.build();
let (_reference, tag_id) = repository.create_annotated_tag("v1.0", &tag, false, 1024 * 1024)?;
assert_eq!(repository.peel_tag(tag_id, 16, 1024 * 1024)?.id, commit);
# Ok(())
# }
```

`create_lightweight_tag` points `refs/tags/<name>` directly at an existing
object. `create_annotated_tag` first verifies that the target exists and matches
the declared type, writes the tag object, then compare-and-swap creates the ref.
Both APIs require explicit `force` to replace a tag. `tags` lists loose and
packed refs in bytewise order, and `delete_tag` uses an expected old ID so a
concurrent replacement is never deleted accidentally.

`peel_tag` follows nested annotated tags, checks every declared type against the
actual object, detects repeated IDs, and applies explicit nesting and object-size
limits. Upload-pack reachability and receive-pack connectivity use this same
strict parser.

## Git source comparisons

- `tag.c:parse_tag_buffer` defines required header order, supported target
  types, optional tagger, and message boundary.
- `tag.c:deref_tag` defines recursive peeling.
- `builtin/tag.c:create_tag` defines canonical serialization and annotated vs.
  lightweight reference creation.
- `refs.c` and the files ref backend define force replacement and deletion
  behavior.

The implementation is independently structured safe Rust using the existing
object and transactional-ref APIs. It does not invoke Git or use gitoxide.
