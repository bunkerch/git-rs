# Revision and object enumeration

`Repository::rev_list` provides structured, bounded reachability enumeration
for pack planning, protocol implementations, maintenance, and application-level
history queries. It works through the repository's configured `FileSystem` and
does not invoke Git.

```rust
use git_rs::{ObjectId, Repository, RevListOptions};

fn objects_for_range(
    repository: &Repository,
    tip: ObjectId,
    already_have: ObjectId,
) -> git_rs::Result<()> {
    let result = repository.rev_list(
        &[tip],
        &[already_have],
        &RevListOptions {
            objects: true,
            ..RevListOptions::default()
        },
    )?;

    for commit in result.commits() {
        println!("commit {}", commit.id());
    }
    for object in result.objects() {
        println!("{:?} {} {:?}", object.kind(), object.id(), object.path());
    }
    Ok(())
}
```

## Commit selection

`rev_list(include, exclude, options)` returns commits reachable from any include
tip and not reachable from an exclude tip. Descendants precede parents in
topological order. `max_count` is applied after exclusions,
`RevListParents::First` limits traversal at every commit, and
`RevListOrder::Reverse` reverses the final output.

With `boundary` enabled, excluded commits directly bordering the selected set
are appended with `RevListSide::Boundary`. Boundary entries retain their parsed
commit metadata but their trees are not included in object enumeration.

`rev_list_symmetric(left, right, options)` implements `left...right`. Each
selected commit is marked `Left` or `Right`, and the result exposes total,
left-side, and right-side counts. Topological output keeps one independent side
together, chooses the side with the newer tip first, and chooses the left side
on a timestamp tie, matching Git's branch-coherent topological traversal.

## Object enumeration

When `objects` is enabled, `RevListResult::objects` contains the deduplicated
tree and blob closure of selected, non-boundary commits. Every entry contains
its kind and the first byte-preserving path through which it was discovered.
Root trees use an empty path. Gitlinks are not traversed into another
repository.

`max_objects` bounds the combined tree/blob set, while `GraphOptions` bounds
commit discovery and individual object sizes. Traversal is iterative, so deeply
nested histories and trees do not consume the Rust call stack.

## Source and compatibility

Selection and output behavior were compared with `builtin/rev-list.c`,
`revision.c`, and `list-objects.c` from upstream Git. Tests cover normal ranges,
symmetric sides, equal-timestamp ordering, boundaries, reversal, object paths,
deduplication, and limits. `examples/rev_list.rs` renders the structured result
in Git's line format and is tested byte-for-byte against `git rev-list
--topo-order` with `--left-right --boundary`, `--reverse`, and `--objects`.
