# Searching tracked content

`Repository::grep_fixed` searches fixed byte strings without a regex engine,
external executable, or host-filesystem assumption. The explicit name prevents
fixed matching from being confused with Git's default basic-regular-expression
syntax.

```rust
# use git_rs::{GrepOptions, GrepTarget, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let matches = repository.grep_fixed(
    &[b"unsafe".to_vec(), b"TODO".to_vec()],
    &GrepOptions {
        target: GrepTarget::Treeish("HEAD".into()),
        ..GrepOptions::default()
    },
)?;
for hit in matches {
    println!("{}:{}", String::from_utf8_lossy(hit.path()), hit.line_number());
}
# Ok(())
# }
```

`Worktree` is the default and searches regular, stage-zero tracked files. An
assume-valid entry is read from its blob, while skip-worktree entries are not
searched. `Index` searches stored blobs and expands sparse-directory index
entries. `Treeish` resolves a commit, tree, or tag and recursively searches its
regular blobs. Gitlinks and symlinks are not searched.

Patterns are byte vectors and are combined with OR semantics. Options provide
ASCII case folding, inverted selection, whole-word matching, literal path
filtering, and Git's default/text/without-match binary
modes. Results retain byte paths and lines plus one-based byte columns and line
numbers. Binary-only matches use zero for both positions.

File count, match count, file size, object size, and tree depth are bounded.
Worktree metadata is checked before allocating file contents. The implementation
adds no dependency.

## Git source comparisons

- `builtin/grep.c:grep_cache` defines worktree versus cached entry selection,
  conflict skipping, assume-valid behavior, and sparse-directory traversal.
- `builtin/grep.c:grep_tree` defines recursive regular-blob tree search.
- `grep.c:grep_source_1` defines line iteration, inversion, first binary-hit
  reporting, and binary modes.
- `grep.c:match_one_pattern` defines fixed-string, case, and word matching.

The code is independently structured safe Rust and invokes no executable.
