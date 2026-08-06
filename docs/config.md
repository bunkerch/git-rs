# Git configuration

`Config` parses and edits repository configuration without assuming host
storage. `Repository::read_config` reads the repository's common `config` file
through its `FileSystem` adapter, and `Repository::write_config` replaces it
under the same atomic lock-and-rename discipline used for references and the
index.

```rust
use git_rs::{InitOptions, MemoryFileSystem, Repository};

let repository = Repository::init(
    MemoryFileSystem::new(),
    "repo",
    &InitOptions::default(),
)?;
let mut config = repository.read_config()?;
config.set("user.name", "Ada Lovelace")?;
config.set("user.email", "ada@example.net")?;
config.add("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
repository.write_config(&config)?;
# Ok::<(), git_rs::Error>(())
```

## Semantics

- Section and variable names are ASCII case-insensitive and normalized to
  lowercase. Modern quoted subsections remain case-sensitive. Legacy dotted
  subsection syntax is accepted with Git's lowercase behavior.
- Values remain bytes rather than requiring UTF-8. Quoting, comments, the
  `\\t`, `\\b`, `\\n`, `\\\\`, and `\\\"` escapes, and backslash-newline
  continuations follow Git's file syntax.
- Repeated variables remain ordered. `get` returns the final assignment,
  `get_all` returns all assignments, `add` appends, and `set` replaces all
  assignments for a key.
- A variable without `=` is represented by `ConfigEntry::value() == None` and
  is boolean true. `get_bool` accepts Git's common true and false spellings;
  `get_i64` accepts signed numbers and binary `k`, `m`, and `g` suffixes.
- Serialization is canonical rather than comment-preserving: assignments and
  multiplicity are retained, while comments and original whitespace are not.

Repository config includes are represented as ordinary `include.path` or
`includeIf.*.path` entries. This API deliberately does not resolve included
files: resolving them requires an explicit policy for paths, conditions, and
the filesystem adapter. Callers can inspect and resolve those entries without
the library silently accessing host-global configuration.

## Source correspondence

Parsing behavior was compared with Git's `config.c`: `get_value` defines value
quotes, comments, whitespace, continuations, and escape handling;
`get_base_var` and the section parser define name normalization and modern and
legacy subsection forms. Integer suffixes and boolean spellings follow Git's
config value conversion rules. The implementation is original Rust code and
does not call or link Git.

Clone remote/branch configuration and linked-worktree repository format
upgrades use this shared API, so the same syntax validation and atomic storage
behavior apply on host, memory, and custom filesystems.
