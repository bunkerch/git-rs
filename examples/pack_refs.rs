use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, PackRefsOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let result = repository.pack_refs(&PackRefsOptions {
        all: arguments.iter().any(|argument| argument == "--all"),
        prune: !arguments.iter().any(|argument| argument == "--no-prune"),
        include: arguments
            .iter()
            .filter_map(|argument| argument.strip_prefix("--include="))
            .map(|value| value.as_bytes().to_vec())
            .collect(),
        exclude: arguments
            .iter()
            .filter_map(|argument| argument.strip_prefix("--exclude="))
            .map(|value| value.as_bytes().to_vec())
            .collect(),
        ..PackRefsOptions::default()
    })?;
    for name in result.packed() {
        println!("packed {name}");
    }
    println!("pruned {} loose refs", result.pruned);
    Ok(())
}
