use std::env;
use std::path::Path;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{
    DeleteBranchOptions, HostFileSystem, ObjectId, RenameBranchOptions, Repository, Signature,
};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let operation = env::args().nth(2).unwrap_or_else(|| "create".to_owned());
    let first = env::args()
        .nth(3)
        .expect("usage: branch <repository> create <name> <object-id> | delete <name> [--force] | rename <old> <new> [--force]");
    match operation.as_str() {
        "create" => {
            let target = env::args().nth(4).expect("create requires an object ID");
            let reference =
                repository.create_branch(&first, ObjectId::from_str(&target)?, false)?;
            println!("created {}", reference.name());
        }
        "delete" => {
            let force = env::args().nth(4).as_deref() == Some("--force");
            let target = repository.delete_branch(
                &first,
                &DeleteBranchOptions {
                    force,
                    ..DeleteBranchOptions::default()
                },
            )?;
            println!("deleted refs/heads/{first} (was {target})");
        }
        "rename" => {
            let new = env::args().nth(4).expect("rename requires a new name");
            let force = env::args().nth(5).as_deref() == Some("--force");
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_secs()
                .try_into()
                .expect("current timestamp must fit in i64");
            let committer = Signature::new("git-rs", "git-rs@example.com", timestamp, 0)?;
            let reference = repository.rename_branch(
                &first,
                &new,
                &RenameBranchOptions { force },
                &committer,
            )?;
            println!("renamed to {}", reference.name());
        }
        other => panic!("unknown branch operation: {other}"),
    }
    Ok(())
}
