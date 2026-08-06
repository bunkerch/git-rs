# Submodules

Submodules are represented by mode `160000` gitlink entries in the
superproject index and `[submodule "name"]` entries in `.gitmodules`.
`git-rs` validates both before accessing a nested path. Absolute paths, empty
components, `.`/`..`, and case-insensitive `.git` components are rejected.
Metadata size and module count are caller-bounded.

```rust
use git_rs::{Repository, SubmoduleOptions};

# fn inspect(repo: &Repository) -> git_rs::Result<()> {
for status in repo.submodule_status(&SubmoduleOptions::default())? {
    println!("{}{}", status.prefix(), String::from_utf8_lossy(status.module().path()));
}
repo.init_submodules(&SubmoduleOptions::default())?;
# Ok(())
# }
```

Status uses Git's leading characters: `-` is uninitialized, a space matches
the index gitlink, `+` has another nested HEAD, and `U` is conflicted. Setting
`cached` compares only `.gitmodules` and the index.

Initialization copies missing URL and safe update values into local config.
Existing values win, and an untrusted `!command` update from `.gitmodules` is
never copied. Update currently implements Git's detached `checkout` policy.
The caller supplies an `UploadPackTransport` for the configured URL, so the
library does not choose a network stack or invoke a process. The child clone
shares the superproject's `Arc<dyn FileSystem>`; memory and custom routed
adapters therefore work without host-path assumptions.

```rust
use git_rs::{Repository, SubmoduleUpdateOptions, UploadPackTransport};

# fn update<T: UploadPackTransport>(repo: &Repository, transport: &mut T) -> git_rs::Result<()> {
repo.update_submodule(
    b"deps/library",
    transport,
    &SubmoduleUpdateOptions { init: true, ..Default::default() },
)?;
# Ok(())
# }
```

The update rejects conflicted or non-gitlink index entries, corrupt existing
nested repositories, unsupported update policies, unavailable target commits,
and checkout conflicts. Pack and inflated-object limits flow through to fetch.

## Source correspondence

The status prefixes, init precedence, and detached checkout behavior are
compared with `Documentation/git-submodule.adoc`, `builtin/submodule--helper.c`,
and `submodule-config.c` in the Git source tree. The implementation is
independent Rust and does not invoke Git or copy gitoxide.

