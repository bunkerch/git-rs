# References and branches

The files reference backend reads loose refs first and falls back to
`packed-refs`. Symbolic refs use the standard `ref: refs/...` representation and
resolution stops after five symbolic hops, matching Git's `SYMREF_MAXDEPTH`.

```rust
use std::str::FromStr;
use git_rs::{InitOptions, MemoryFileSystem, ObjectId, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    "project",
    &InitOptions::default(),
)?;
let target = ObjectId::from_str("1111111111111111111111111111111111111111")?;
repository.create_branch("feature/storage", target, false)?;
assert_eq!(repository.resolve_reference("refs/heads/feature/storage")?, target);
# Ok::<(), git_rs::Error>(())
```

`create_branch(..., false)` is create-only. Passing `true` allows replacement.
The lower-level `update_reference` API also supports exact compare-and-swap with
`PreviousValue::MustExist`, which is useful when a caller must reject a stale
write.

Updates exclusively acquire `<ref>.lock`, inspect the current loose or packed
value while holding that lock, write the new complete value, and atomically
rename the lock over the loose ref. Failed transactions remove their lock.

## Git source comparisons

The implementation is independent Rust code. Behavior was derived and tests are
organized around these upstream contracts:

- `refs.c:check_refname_component` and `check_refname_format` define forbidden
  bytes, components, `.lock`, `..`, and `@{` restrictions.
- `refs/refs-internal.h:SYMREF_MAXDEPTH` defines the five-hop symbolic limit.
- `refs/files-backend.c:read_ref_internal` defines loose-first, packed fallback.
- `refs/files-backend.c` and `lockfile.c` define exclusive `.lock` acquisition
  and atomic publication.
- `hex.c:get_oid_hex` and `hash_to_hex_algop_r` define object-ID parsing and
  canonical lowercase formatting.

The current repository format is version 0 and therefore uses 20-byte SHA-1
object IDs. SHA-256 repository-format support will be represented explicitly
rather than accepting ambiguous identifier lengths.

The host-backed example opens an existing repository and creates a branch:

```console
cargo run --example branch -- my-repository feature/new <40-hex-object-id>
```
