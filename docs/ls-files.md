# Listing index and worktree files

`Repository::ls_files` exposes Git's common index and worktree inventory modes
without invoking an executable. Paths remain byte vectors, index stages and
flags remain typed metadata, and all reads go through the repository filesystem
adapter.

```rust
# use git_rs::{LsFilesKind, LsFilesOptions, Repository};
# fn example(repository: &Repository) -> git_rs::Result<()> {
let entries = repository.ls_files(&LsFilesOptions {
    modified: true,
    deleted: true,
    others: true,
    ..LsFilesOptions::default()
})?;
for entry in entries {
    println!("{:?}: {}", entry.kind(), String::from_utf8_lossy(entry.path()));
}
# Ok(())
# }
```

With no selector, cached entries are returned. `stage` includes index metadata;
`unmerged` restricts output to conflict stages 1–3. `modified` includes missing
tracked paths, while `deleted` reports those paths separately, so combining the
two can intentionally produce two classifications. `others`, `ignored`, and
`killed` scan the worktree. `exclude_standard` enables repository, per-directory,
and user exclusion files like Git's `--exclude-standard`; ignored mode requires
this exclusion source and either cached or other-file selection.

Sparse-directory index entries are expanded into their tree contents by
default. Set `show_sparse_directories` to expose the sparse entry itself, like
Git's `--sparse`. `paths` are literal repository-relative byte paths and select
the named path or descendants; `error_unmatch` rejects unmatched selections.
Index expansion, object size, directory depth, and scanned-entry counts are
bounded by the options.

## Git source comparisons

- `builtin/ls-files.c:cmd_ls_files` defines selector defaults and option
  interactions.
- `builtin/ls-files.c:show_files` defines other/killed-before-index ordering and
  the independent cached, deleted, modified, and unmerged classifications.
- `builtin/ls-files.c:show_ce` defines sparse-directory expansion and `--sparse`.
- `read-cache.c:ce_match_stat_basic` defines worktree type, mode, and content
  comparison semantics shared with status.
- `dir.c:fill_directory` and `dir.c:is_excluded` define worktree enumeration and
  ignore filtering.

The implementation is independently structured safe Rust, adds no dependency,
and invokes no executable.
