# Ref-relative object names

`Repository::name_revs` assigns human-readable names such as `main~2`,
`tags/v2^0`, or `main~3^2~4` to object IDs. It returns structured entries in
the same order as the requested IDs and works over every `FileSystem` adapter.

```rust
use git_rs::{NameRevOptions, ObjectId, Repository};

fn names(repository: &Repository, ids: &[ObjectId]) -> git_rs::Result<Vec<String>> {
    Ok(repository
        .name_revs(ids, &NameRevOptions {
            always: true,
            allow_undefined: false,
            ..NameRevOptions::default()
        })?
        .into_iter()
        .map(|entry| entry.name().expect("always supplies a fallback").to_owned())
        .collect())
}
```

Tag tips outrank non-tags. Competing tag names prefer the lowest effective
distance; non-tags prefer distance and then the older tip date. First-parent
walks use `~N`, while second and later merge parents use `^N`. Annotated tags
name their commits with `^0`, and refs pointing directly at blobs, trees, or tag
objects provide exact names.

`tags_only`, inclusive and exclusive wildcard patterns, optional tag-basename
shortening, target-date cutoff control, and unique-abbreviation fallback are
available through `NameRevOptions`. Wildcard filters test both complete refnames
and slash-delimited suffixes, as Git does.

As in Git, `allow_undefined` defaults to true and takes precedence over
`always`. Set `allow_undefined` to false with `always` to request abbreviation
fallback; set both false to make an unnamed object an error.

Reference enumeration, nesting depth, commit count, object size, traversal
updates, and abbreviation candidates are independently bounded. The traversal
is iterative, so deep histories do not consume the Rust call stack.

## Git source comparison

The implementation follows the algorithms, without copying code, in
`/home/coder/git/builtin/name-rev.c`:

- `effective_distance` and `is_better_name` for tag/non-tag priority.
- `get_parent_name` and `name_rev` for first-parent and merge-parent notation.
- `subpath_matches` and `name_ref` for ref filtering and shortening.
- `cmp_by_tag_and_age` for deterministic tip processing priority.
- `get_exact_ref_match` and `get_rev_name` for non-commit and rendered names.
- `CUTOFF_DATE_SLOP` for bounded target-oriented history traversal.

Run the host adapter example with:

```sh
cargo run --example name_rev -- /path/to/repository --no-undefined --always OBJECT_ID...
```
