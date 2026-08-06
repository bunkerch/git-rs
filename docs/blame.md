# Blame

`Repository::blame` attributes each line of a stored file to the commit which
introduced it. It operates directly on commits, trees, and blobs through the
configured `FileSystem`; no worktree or Git process is required.

```rust
# use git_rs::{BlameOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
for line in repository.blame("HEAD", b"src/lib.rs", &BlameOptions::default())? {
    println!(
        "{} {} {} {}",
        line.commit(),
        line.original_line(),
        line.final_line(),
        line.author().name(),
    );
}
# Ok(())
# }
```

Line numbers are one-based. `contents` preserves the final line's exact bytes,
including its newline when present. `original_path` follows exact whole-file
renames. Root-origin lines have `is_boundary() == true`.

For merge commits, each line is passed to the first parent containing the same
line according to the Myers edit mapping; later parents receive lines which do
not survive through earlier parents. Set `first_parent` to inspect only the
first-parent history.

Every potentially expensive dimension is explicit: `max_commits`,
`max_tree_entries`, `max_lines`, `max_trace_cells`, and the object/revision
limits nested in `revision`. Attribution uses the same bounded Myers engine as
patch generation. Unchanged blobs use an identity mapping without diff work,
and the result remains in final-file order.

## Git source comparison

- `blame.c:pass_blame_to_parent` and `pass_blame` define passing unchanged
  ranges from a suspect commit to its parents.
- `blame.c:find_origin` and `find_rename` define same-path lookup and rename
  following. This API currently follows exact whole-file renames, which are
  unambiguous and require no similarity heuristic.
- `builtin/blame.c:emit_porcelain` defines commit, original-line, final-line,
  path, author, boundary, and content concepts represented by `BlameLine`.
- `xdiff` supplies Git's line correspondence; git-rs uses its independently
  implemented bounded Myers correspondence engine.
