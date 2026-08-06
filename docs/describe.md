# Describing commits

`Repository::describe` names a commit from the nearest eligible reachable tag
or reference and returns both the rendered Git form and typed components.

```rust
# use git_rs::{DescribeOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let description = repository.describe("HEAD", &DescribeOptions::default())?;
println!("{}", description.rendered());
println!("distance: {} commits", description.depth());
# Ok(())
# }
```

Annotated tags are eligible by default. `tags` admits lightweight tags; `all`
admits branches, remotes, and other refs and renders their name below `refs/`.
Annotated tags outrank lightweight tags and other refs at the same commit. Two
annotated tags at one commit are resolved by newer tagger timestamp and then
stable bytewise ref order. The annotated tag header supplies the displayed name,
including Git's forced long form for a mismatched external ref name.

Candidate discovery follows commit-date priority in the target's reachable
graph and stops at `max_candidates`. Each candidate distance is the exact number
of commits reachable from the target but not the candidate. `first_parent`
restricts both discovery and distance. `exact_match`, `long`, zero abbreviation,
`always`, include patterns, and exclude patterns mirror their Git options.

`Description` exposes the chosen name, target ID, distance, unique abbreviation,
exact-match state, annotated state, and final rendered string. Reference count,
reference depth, tag depth, candidate count, commit count, object size, and
abbreviation inspection are bounded. No dependency or executable is used.

## Git source comparisons

- `builtin/describe.c:get_name` defines ref filtering, peeling, and name
  priorities.
- `builtin/describe.c:replace_name` defines annotated tagger-date tie breaking.
- `builtin/describe.c:describe_commit` defines exact handling, commit-date
  candidate discovery, candidate bounds, first-parent traversal, and fallback.
- `builtin/describe.c:append_name` and `append_suffix` define displayed names and
  `<name>-<depth>-g<abbrev>` rendering.
- `object-name.c:repo_find_unique_abbrev` defines collision-extending object
  abbreviations.

The implementation is independently structured safe Rust over abstract storage.
