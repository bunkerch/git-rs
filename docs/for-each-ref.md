# Structured reference inventory

`Repository::for_each_ref` provides the selection and ordering core of Git's
`for-each-ref` as typed Rust data. It does not require a format-string parser,
and it works identically with host, memory, or custom `FileSystem` adapters.

```rust
use git_rs::{ForEachRefOptions, RefSortField, RefSortKey, Repository};

fn newest_release_refs(repo: &Repository) -> git_rs::Result<Vec<String>> {
    let entries = repo.for_each_ref(&ForEachRefOptions {
        patterns: vec!["refs/tags/releases".into()],
        sort: vec![RefSortKey::descending(RefSortField::CreatorDate)],
        count: Some(20),
        ..ForEachRefOptions::default()
    })?;
    Ok(entries.into_iter().map(|entry| entry.name().to_owned()).collect())
}
```

The result exposes direct and peeled object IDs/types, symbolic targets, and
commit/tag identities. Filters include positive/excluded path patterns,
case-folding, direct or intermediate annotated-tag `points_at`, commit
`contains`/`no_contains`, `merged_into`/`not_merged_into`, count limits, and
lexicographic pagination. Multiple sort keys follow Git's rule that the last
specified key is primary.

All enumeration, tag peeling, object reads, and graph walks have caller-visible
limits. Missing or corrupt history is returned as an error rather than being
silently classified as a non-match.

## Git source comparison

The implementation follows these upstream areas in `/home/coder/git`:

- `builtin/for-each-ref.c` for option compatibility and pagination constraints.
- `ref-filter.c:2670` (`match_pattern`/`match_name_as_path`) for literal path and
  wildcard matching.
- `ref-filter.c:2840` (`match_points_at`) for checking every annotated-tag layer.
- `ref-filter.c:2973` (`apply_ref_filter`) for filter ordering and commit peeling.
- `ref-filter.c:3139` (`reach_filter`) for merged and not-merged reachability.

The library deliberately returns structured fields rather than Git's shell,
Perl, Python, Tcl, color, and arbitrary `--format` presentation modes. Those
are serialization concerns and can be implemented by the calling Rust code.

Run the host-filesystem example with:

```sh
cargo run --example for_each_ref -- /path/to/repository refs/heads
```
