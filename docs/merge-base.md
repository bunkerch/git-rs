# Merge bases and ancestry

`git-rs` exposes Git's commit-topology queries directly on `Repository`. They
read commits, shallow boundaries, replacement objects, commit graphs, refs, and
reflogs through the configured `FileSystem`; they do not invoke Git or require a
host filesystem.

```rust
use git_rs::{GraphOptions, ObjectId, Repository};

fn inspect(
    repository: &Repository,
    topic: ObjectId,
    main: ObjectId,
) -> git_rs::Result<()> {
    let limits = GraphOptions::default();
    let bases = repository.merge_bases(topic, main, &limits)?;
    let already_merged = repository.is_ancestor(topic, main, &limits)?;
    println!("bases={bases:?}, already merged={already_merged}");
    Ok(())
}
```

## Query variants

- `merge_bases(one, two, options)` returns every best common ancestor. Multiple
  results are possible for criss-cross histories.
- `merge_bases_many(one, others, options)` implements ordinary multi-argument
  `git merge-base`: `others` are the parents of a hypothetical merge commit, so
  ancestry reachable from any of them participates.
- `octopus_merge_bases(commits, options)` finds bases suitable for one n-way
  merge. This is deliberately distinct from the ordinary multi-argument form.
- `is_ancestor(ancestor, descendant, options)` includes equality.
- `independent_commits(commits, options)` removes duplicates and every input
  reachable from another input.
- `fork_point(reference, derived, options)` considers the reference's reflog.
  It returns a value only if the unique merge base is an actual current or
  historical ref tip. Full refnames and Git-style branch/tag/remote shorthand
  are accepted.

All traversals are bounded by `GraphOptions::max_commits` and
`max_object_size`. `ForkPointOptions` additionally bounds reflog entries.
Shallow commits terminate ancestry, replacement objects are honored, and an
available commit graph accelerates ancestry checks when those semantics permit
it.

## Source comparison

The ordinary, octopus, independent, ancestor, and fork-point semantics were
derived from `builtin/merge-base.c`, `commit-reach.c`, and `get_fork_point()` in
`commit.c` in the upstream Git source. Unit tests cover linear, criss-cross,
hypothetical-merge, octopus, independent-head, reflog-rewrite, DWIM-ref, and
resource-limit behavior. The `examples/merge_base.rs` program can run the same
queries against a normal Git repository for byte-level output comparison with
`git merge-base`.
