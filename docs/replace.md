# Replace objects

Replace refs redirect object reads without changing the original object ID.
`git-rs` implements the standard `refs/replace/<original-id>` representation,
so replacements written through any filesystem adapter are understood by Git
when the repository is materialized on disk.

```rust
# use git_rs::{ObjectKind, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let old = repository.write_object(ObjectKind::Blob, b"old")?;
let new = repository.write_object(ObjectKind::Blob, b"new")?;
repository.create_replacement(old, new, false, 1024)?;

assert_eq!(repository.read_object(old, 1024)?.data(), b"new");
assert_eq!(repository.read_object_raw(old, 1024)?.data(), b"old");
# Ok(())
# }
```

`read_object` follows replacement chains for every higher-level parser built
on it. `read_object_raw` deliberately bypasses replacement lookup for integrity
checking, pack delta bases, and replacement administration. Chains use Git's
five-hop bound; cycles and longer chains are errors.

The lazy replacement map is shared by repository clones for fast repeated
object reads. `create_replacement`, `delete_replacement`, generic direct ref
updates/deletions, and `write_config` invalidate it. Call
`invalidate_replacements` after an out-of-band storage client changes replace
refs or configuration. `core.useReplaceRefs=false` disables transparent reads,
while `replacements` still lists the stored refs.

Without `force`, creation requires matching object kinds and refuses an
existing ref. Updates and deletion use compare-and-swap reference operations.

## Git source comparisons

- `replace-object.c:prepare_replace_object` builds the original-to-replacement
  map from `refs/replace/`.
- `replace-object.c:do_lookup_replace_object` defines recursive lookup and
  `MAXREPLACEDEPTH` of five.
- `replace-object.h:replace_refs_enabled` defines the
  `core.useReplaceRefs=false` behavior.
- `builtin/replace.c:replace_object_oid` defines kind validation, force, and
  transactional ref replacement.

The implementation is independently structured safe Rust, uses the existing
abstract reference/object stores, and invokes no executable or native library.
