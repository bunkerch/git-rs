# Archiving stored trees

`Repository::archive` produces deterministic TAR or ZIP bytes directly from a
commit, tree, or annotated-tag revision. It reads objects through the configured
`FileSystem`; a bare or remotely backed repository works without materializing a
worktree or invoking another process.

```rust
use git_rs::{ArchiveFormat, ArchiveOptions, Repository};

fn zip(repository: &Repository) -> git_rs::Result<Vec<u8>> {
    repository.archive("HEAD", &ArchiveOptions {
        format: ArchiveFormat::Zip,
        prefix: b"project-1.0".to_vec(),
        paths: vec![b"src".to_vec(), b"README.md".to_vec()],
        ..ArchiveOptions::default()
    })
}
```

Selections are literal repository-relative paths and recursively include a
selected directory. Every selection must match; absolute paths, parent
components, NULs, and unmatched paths are rejected. `prefix` is normalized and
validated by the same rules.

Both formats preserve regular versus executable files, directories, symlink
targets, gitlinks as empty directories, byte-valued names, and the commit's
committer timestamp. A caller can override the timestamp for reproducible
artifacts. TAR uses POSIX PAX extended headers when a path or symlink target does
not fit USTAR. ZIP uses stored entries, CRC-32, extended Unix timestamps, UTF-8
flags when applicable, and Unix mode/type fields. Stored ZIP output avoids a
second compression dependency and avoids wasting CPU on blobs that are already
compressed in the object database.

Archive traversal evaluates `.gitattributes` from the archived tree by default.
Paths with `export-ignore` set are omitted; ignored directories are pruned
before their objects are read. Regular files with `export-subst` set expand
`$Format:...$` placeholders from the archived commit. The formatter supports
full and abbreviated commit/tree/parent IDs, author and committer identity and
timestamps, subject/body/raw message, sanitized subject, literal hex bytes,
newlines, percent literals, and `%(describe)`. Unknown pretty-format codes are
preserved, matching Git's formatter. Tree-only archives cannot substitute
commit metadata.

Set `worktree_attributes` to use the live worktree with index fallback, matching
`git archive --worktree-attributes`. This is rejected for a bare repository.
Attribute source counts, bytes, rules, macro depth, and substitution growth are
bounded by `ArchiveOptions`.

`max_object_size`, `max_archive_size`, and `max_entries` bound reads, allocation,
and traversal. Output is returned only after the complete archive succeeds.

```console
cargo run --example archive -- /path/to/repository HEAD source.tar
cargo run --example archive -- /path/to/repository v1 source.zip \
  --zip --prefix=project-1.0 src README.md
cargo run --example archive -- /path/to/repository HEAD source.tar \
  --worktree-attributes
```

## Git source comparison

Tree traversal, literal recursive pathspec behavior, gitlink directories, and
mode classification correspond to `write_archive_entries` in `archive.c`.
`queue_or_write_archive_entry`, `get_archive_attrs`, `object_file_to_archive`,
and `format_subst` in `archive.c` are the comparison points for directory
pruning, attribute precedence, and commit placeholder expansion.
USTAR/PAX field behavior follows `write_tar_entry` in `archive-tar.c`; ZIP local
headers, central records, CRCs, UTF-8 signaling, and Unix attributes correspond
to `write_zip_entry` in `archive-zip.c`. The implementation is independently
structured Rust and shares only the documented archive formats and Git object
semantics with those sources.
