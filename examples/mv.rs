use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, MoveOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let source = env::args()
        .nth(2)
        .expect("usage: mv <repository> <source> <destination> [--force] [--dry-run]");
    let destination = env::args()
        .nth(3)
        .expect("usage: mv <repository> <source> <destination> [--force] [--dry-run]");
    let arguments = env::args().skip(4).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let count = repository.move_path(
        source,
        destination,
        &MoveOptions {
            force: arguments.iter().any(|argument| argument == "--force"),
            dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
            ..MoveOptions::default()
        },
    )?;
    println!("moved {count} tracked path(s)");
    Ok(())
}
