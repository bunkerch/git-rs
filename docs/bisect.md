# Bisection

The bisect API stores its session in Git's `refs/bisect/*`, `BISECT_START`,
`BISECT_TERMS`, `BISECT_NAMES`, `BISECT_LOG`, `BISECT_HEAD`, and
`BISECT_EXPECTED_REV` layout. A session started in memory can therefore be
materialized and inspected or continued by Git, and the inverse works as well.

```rust
# use git_rs::{BisectOptions, Repository, Signature};
# fn example(repository: &Repository, bad: git_rs::ObjectId, good: git_rs::ObjectId) -> git_rs::Result<()> {
let identity = Signature::new("Tester", "tester@example.com", 1, 0)?;
let outcome = repository.start_bisect(
    bad,
    &[good],
    &BisectOptions { no_checkout: true, ..BisectOptions::default() },
    &identity,
)?;
println!("{outcome:?}");
# Ok(())
# }
```

`start_bisect` validates that every good commit is an ancestor of the bad tip,
publishes the initial refs as one CAS transaction, and selects a midpoint.
`mark_bisect` records `Good`, `Bad`, or `Skip`; implicit marks require the
current commit to equal `BISECT_EXPECTED_REV`, preventing accidental results
after an out-of-band checkout. Explicit marks are validated inside the current
ancestry interval before state changes.

For ordinary single-parent history, midpoint weights are computed in one
reverse pass. Only merge commits require a unique-ancestor walk, bounded by
`max_merge_ancestor_visits`. Overall graph parsing is bounded by `GraphOptions`.
Ties follow deterministic topological order. Outcomes distinguish a commit to
test, the proven first bad commit, and a range containing only skipped commits.

With checkout enabled, candidate selection uses the existing protected detached
checkout and refuses local-overwrite conflicts. No-checkout mode updates
`BISECT_HEAD` instead. `reset_bisect` restores the original branch or detached
commit only when checkout mode moved HEAD, then removes all bisect refs and
state files.

The API currently uses Git's default `bad` and `good` terms. It detects custom
terms and returns an explicit error rather than interpreting their refs
incorrectly.

## Git source comparisons

- `builtin/bisect.c:bisect_start`, `bisect_state`, and `bisect_reset` define
  durable state, marking, and cleanup.
- `bisect.c:do_find_bisection` defines linear-parent weighting and merge
  ancestor counting.
- `bisect.c:filter_skipped` defines skipped-candidate outcomes.
- `bisect.c:bisect_checkout` defines checkout versus `BISECT_HEAD` behavior and
  `BISECT_EXPECTED_REV` protection.

The implementation is independently structured safe Rust, routes every read
and write through repository abstractions, adds no dependency, and invokes no
executable.
