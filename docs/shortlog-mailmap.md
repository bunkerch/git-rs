# Shortlog and mailmaps

`Repository::shortlog` creates bounded contributor summaries directly from
stored commit objects. It can group each commit by author, committer, and one
or more case-insensitive message trailer keys. Multiple group sources are
deduplicated per commit. Entries are alphabetic by default or descending by
commit count with alphabetic ties, and subjects are returned oldest first.

```rust
use git_rs::{Repository, ShortlogGroup, ShortlogOptions};

# fn summarize(repository: &Repository, head: git_rs::ObjectId)
#     -> git_rs::Result<()> {
let entries = repository.shortlog(
    &[head],
    &[],
    &ShortlogOptions {
        groups: vec![
            ShortlogGroup::Author,
            ShortlogGroup::Trailer("co-authored-by".into()),
        ],
        include_email: true,
        sort_by_number: true,
        ..ShortlogOptions::default()
    },
)?;
for entry in entries {
    println!("{} {}", entry.commit_count(), String::from_utf8_lossy(entry.identity()));
}
# Ok(())
# }
```

Subjects strip a leading `[PATCH...]` marker and collapse a multiline first
paragraph. Trailer identities are parsed as `name <email>` where possible;
literal non-identity values remain byte-preserving group names. Duplicate
trailer values in one commit count only once.

## Mailmaps

`Mailmap::parse` supports all four forms documented by `gitmailmap(5)`, with
case-insensitive email and optional name matching. `Repository::load_mailmap`
reads sources through the configured `FileSystem` in Git's override order:

1. `.mailmap` at the worktree root;
2. `mailmap.blob`, or `HEAD:.mailmap` by default for a bare repository;
3. `mailmap.file`.

This works with host, memory, and custom storage adapters. Parsing has explicit
byte and entry limits; shortlog additionally bounds graph discovery, group
count, object sizes, diff work for selected paths, and aggregate subject bytes.

Run the host-backed count summary example with:

```console
cargo run --example shortlog -- path/to/repository HEAD
```

Behavioral comparisons use `builtin/shortlog.c` for group deduplication,
subject cleanup, sorting, and output order; `mailmap.c` for mapping precedence;
and `Documentation/gitmailmap.adoc` for accepted mapping forms.
