use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository, RestoreOptions, RestoreTarget};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let staged = arguments.iter().any(|argument| argument == "--staged");
    let worktree = arguments.iter().any(|argument| argument == "--worktree");
    let target = match (staged, worktree) {
        (true, true) => RestoreTarget::Both,
        (true, false) => RestoreTarget::Index,
        (false, _) => RestoreTarget::Worktree,
    };
    let source = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--source="))
        .map(ObjectId::from_str)
        .transpose()?;
    let paths = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    assert!(
        !paths.is_empty(),
        "usage: restore <repository> <path>... [--staged] [--worktree] [--source=<object-id>] [--force] [--dry-run]"
    );

    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let restored = repository.restore_paths(
        &paths,
        &RestoreOptions {
            source,
            target,
            force: arguments.iter().any(|argument| argument == "--force"),
            dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
            ..RestoreOptions::default()
        },
    )?;
    for path in restored {
        println!("restored '{}'", String::from_utf8_lossy(&path));
    }
    Ok(())
}
