use std::env;
use std::path::{Path, PathBuf};

use git_rs::{HostFileSystem, RemoveOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let paths = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    assert!(
        !paths.is_empty(),
        "usage: rm <repository> <path>... [--cached] [--force] [--recursive] [--dry-run]"
    );
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let removed = repository.remove(
        &paths,
        &RemoveOptions {
            cached: arguments.iter().any(|argument| argument == "--cached"),
            force: arguments.iter().any(|argument| argument == "--force"),
            recursive: arguments.iter().any(|argument| argument == "--recursive"),
            dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
            ..RemoveOptions::default()
        },
    )?;
    for path in removed {
        println!("rm '{}'", String::from_utf8_lossy(&path));
    }
    Ok(())
}
