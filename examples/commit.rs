use std::env;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{CommitOptions, HostFileSystem, Repository, Signature};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let message = env::args()
        .nth(2)
        .unwrap_or_else(|| "commit from git-rs\n".to_owned());
    let amend = env::args().skip(3).any(|argument| argument == "--amend");
    let allow_empty = env::args()
        .skip(3)
        .any(|argument| argument == "--allow-empty");
    let name = env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "git-rs".to_owned());
    let email = env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "git-rs@example.com".to_owned());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| git_rs::Error::InvalidCommit(error.to_string()))?
        .as_secs()
        .try_into()
        .map_err(|_| git_rs::Error::InvalidCommit("current timestamp exceeds i64".into()))?;

    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let identity = Signature::new(name, email, timestamp, 0)?;
    let commit = repository.commit_index(
        message.as_bytes(),
        &identity,
        &identity,
        &CommitOptions {
            amend,
            allow_empty,
            ..CommitOptions::default()
        },
    )?;
    println!("{commit}");
    Ok(())
}
