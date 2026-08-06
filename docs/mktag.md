# Strict tag-object creation

`Repository::mk_tag` validates caller-supplied annotated-tag bytes with
`mktag`-style fsck rules, verifies the declared target, and stores the exact
input as a tag object.

```rust
use git_rs::MkTagOptions;
# fn example(repository: &git_rs::Repository<impl git_rs::FileSystem>, target: git_rs::ObjectId) -> git_rs::Result<()> {
let input = format!(
    "object {target}\ntype commit\ntag v1.0\ntagger Release <release@example.com> 1 +0000\n\nready\n"
);
let id = repository.mk_tag(input.as_bytes(), &MkTagOptions::default())?;
println!("{id}");
# Ok(())
# }
```

Required headers must be ordered `object`, `type`, `tag`, then optional
`tagger`. Header-only objects ending after the final LF and objects with a
blank-line-separated message are both accepted. Target IDs are read without
replacement rewriting and must exist with the declared blob, tree, commit, or
tag type.

Strict mode, enabled by default, promotes Git's tag fsck warnings to errors:
the tagger must exist, the tag name must form a valid `refs/tags/*` reference,
and only a single `gpgsig` or `gpgsig-sha256` signature header may follow it.
`strict: false` accepts historical warning-level forms but still rejects broken
headers, identities, IDs, types, missing targets, and type mismatches.

Input and target sizes are independently bounded. `dry_run` validates and
returns the content-derived ID without publishing an object. Exact input bytes,
including messages and signature continuations, are never normalized. Parsed
identity timezones retain historical four-digit spellings such as `+9999`.

The implementation follows `builtin/mktag.c`, `fsck.c:fsck_tag_standalone`,
and `Documentation/git-mktag.adoc`. It uses the configured object filesystem
and never invokes Git. The host example reads a bounded tag body from stdin:

```text
cargo run --example mktag -- repository <tag-body
```
