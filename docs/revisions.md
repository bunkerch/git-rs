# Revision graph

`Repository::walk_revisions` walks commits reachable from one or more included
tips while hiding commits reachable from excluded tips. Output is topological:
every descendant appears before its parents. Commits that are simultaneously
eligible are ordered by committer timestamp and object ID, giving deterministic
results even when timestamps match.

```rust
# use git_rs::{Repository, RevisionWalkOptions};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let head = repository.resolve_reference("HEAD")?;
for revision in repository.walk_revisions(&[head], &[], &RevisionWalkOptions::default())? {
    println!("{} {}", revision.id(), String::from_utf8_lossy(revision.commit().message()));
}
# Ok(())
# }
```

`RevisionWalkOptions::first_parent` follows only the first parent on both the
included and excluded sides. `max_count` limits returned results after graph
discovery. `GraphOptions::max_commits` limits discovery itself, and
`max_object_size` bounds each commit read. Traversal is iterative and caches
each parsed commit once per query.

`Repository::is_ancestor` performs an inclusive bounded reachability query.
`Repository::merge_bases` returns every best common ancestor: no returned base
is an ancestor of another returned base. This preserves multiple bases for
criss-cross histories instead of arbitrarily discarding one.

Push uses the same bounded ancestry implementation for non-fast-forward checks.
These APIs depend only on repository object reads and therefore work unchanged
with every filesystem adapter.

## Git source comparisons

- `revision.c` and `list-objects.c` define include/exclude traversal and
  first-parent behavior.
- `commit-reach.c:paint_down_to_common` and `get_merge_bases_many_0` define
  common-ancestor painting and removal of ancestors of better bases.
- `commit.c:commit_list_insert_by_date` defines commit-date ordering used as a
  topological tie-break.

The implementation is independently expressed in safe Rust using owned commit
values, hash maps, a priority queue, and explicit resource limits. It does not
invoke Git or use gitoxide.
