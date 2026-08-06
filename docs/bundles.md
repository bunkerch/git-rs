# Bundles

`GitBundle` implements Git bundle versions 2 and 3 as an in-memory transport
artifact. Parsing does not touch a filesystem; creation and import access
repository state only through its configured `FileSystem` adapter.

```rust
# use git_rs::{BundleCreateOptions, BundleParseOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let bundle = repository.create_bundle(&BundleCreateOptions {
    references: vec!["refs/heads/main".into()],
    ..BundleCreateOptions::default()
})?;
let bytes = bundle.encode();
let parsed = git_rs::GitBundle::parse(&bytes, &BundleParseOptions::default())?;
assert_eq!(parsed.references()[0].name(), "refs/heads/main");
# Ok(())
# }
```

Creation resolves every named ref, walks its complete commit/tree/tag/blob
closure, subtracts the closure of prerequisite commits, and builds a
deterministic pack. Traversal is bounded while it runs by `max_objects`; object
loads are bounded by `max_object_size`. Prerequisite subjects use the first
commit-message line and cannot inject header records.

Parsing bounds the header, pack, references, and prerequisites before
allocation. V3 `object-format=sha1` is supported. Unknown capabilities and
filtered bundles are rejected: a filtered graph needs promisor-object semantics
and is not misrepresented as a complete import.

`verify_bundle` checks that prerequisite commits and their histories exist,
validates the incoming pack (including thin deltas against existing storage),
requires every advertised tip, and checks typed graph edges. `unbundle` then
publishes the validated pack using the existing pack-before-index atomic
boundary. Matching Git, unbundle does not choose or update destination refs;
callers inspect `GitBundle::references` and apply their own ref transaction.

## Git source comparisons

- `bundle.c:read_bundle_header_fd` defines signatures, capabilities,
  prerequisites, advertised refs, and the blank-line boundary.
- `bundle.c:write_bundle_prerequisites` and `write_bundle_refs` define canonical
  header records.
- `bundle.c:create_bundle` defines prerequisite subtraction and pack creation.
- `bundle.c:verify_bundle` defines prerequisite connectivity checks.
- `bundle.c:unbundle` defines verified pack import without ref updates.

The implementation is independently structured safe Rust, adds no dependency,
and invokes neither Git nor another executable.
