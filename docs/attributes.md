# Git attributes

`Repository::check_attributes` is a storage-agnostic attribute engine for
operations such as `check-attr`, archive selection, diff/merge drivers, and
future clean/smudge filters. It returns structured `AttributeResult` values;
the four possible states are `Set`, `Unset`, `Value(String)`, and
`Unspecified`.

```rust
use git_rs::{AttributeValue, CheckAttributesOptions, Repository};

fn is_binary(repository: &Repository, path: &[u8]) -> git_rs::Result<bool> {
    let results = repository.check_attributes(
        &[path.to_vec()],
        &CheckAttributesOptions {
            attributes: vec!["text".into()],
            ..CheckAttributesOptions::default()
        },
    )?;
    Ok(matches!(results[0].value(), AttributeValue::Unset))
}
```

The default source checks each applicable worktree `.gitattributes` and falls
back to its stage-zero index entry when the worktree file is absent. Set
`source` to `AttributeSource::Index` for cached or bare-repository queries.
Use `AttributeSource::Tree(tree_id)` to query a historical tree directly.
Root rules are applied first, then successively deeper files, and finally
`$GIT_DIR/info/attributes`, matching Git's precedence.

Supported syntax includes C-quoted patterns, comments, slash-relative and
basename patterns, `*`, `**`, `?`, bracket classes, set/unset/value/unspecified
states, root and info attribute macros, the built-in `binary` macro, and
`builtin_objectmode`. Negative patterns and reserved user-defined `builtin_*`
attributes are rejected. Trailing-slash directory patterns are ineffective, as
in Git; use `directory/**` to affect descendants.

Host-global and system attribute files are intentionally outside this API.
Reading them would violate the storage boundary for memory, S3, database, and
other custom `FileSystem` implementations. Applications may model such policy
inside their adapter or repository-local sources.

Every query bounds paths, result rows, source files, source size, rules, and
macro recursion. Malformed data and exceeded bounds return errors.

## Git source comparison

The implementation was derived from these areas of `/home/coder/git`:

- `builtin/check-attr.c` for query states and cached selection.
- `attr.c:199` for attribute-name validation.
- `attr.c:265` and `attr.c:321` for assignment and line parsing.
- `attr.c:419` and `attr.c:951` for source ordering and precedence.
- `attr.c:1060` for pathname matching.
- `attr.c:1089` for per-attribute matching and macro expansion.
- `Documentation/gitattributes.adoc` for public pattern and state semantics.

Run the host adapter example with:

```sh
cargo run --example check_attributes -- /path/to/repository path/to/file text diff
```
