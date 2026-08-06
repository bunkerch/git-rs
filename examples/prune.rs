use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, PruneOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let force = arguments.iter().any(|argument| argument == "--force");
    let expire_before = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--expire-before="))
        .map(str::parse::<u64>)
        .transpose()
        .expect("--expire-before must be Unix seconds");
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let entries = repository.prune(&PruneOptions {
        expire_before,
        prune_packed_copies: !arguments
            .iter()
            .any(|argument| argument == "--keep-packed-copies"),
        force,
        dry_run: !force,
        ..PruneOptions::default()
    })?;
    for entry in entries {
        let id = entry
            .id()
            .map_or_else(|| "temporary".to_owned(), |id| id.to_string());
        let action = if force { "removed" } else { "would remove" };
        println!(
            "{action} {id} {:?} {}",
            entry.reason(),
            entry.path().display()
        );
    }
    Ok(())
}
