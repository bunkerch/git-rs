use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{HostFileSystem, Repository, Signature, SwitchOptions};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let branch = env::args()
        .nth(2)
        .expect("usage: switch <repository> <branch> [--force]");
    let force = env::args().any(|argument| argument == "--force");
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    repository.switch_branch(
        &branch,
        &SwitchOptions {
            force,
            ..SwitchOptions::default()
        },
        &identity()?,
    )?;
    Ok(())
}

fn identity() -> git_rs::Result<Signature> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| git_rs::Error::InvalidReference(error.to_string()))?
        .as_secs()
        .try_into()
        .map_err(|_| git_rs::Error::InvalidReference("timestamp overflow".into()))?;
    Signature::new("git-rs", "git-rs@example.com", timestamp, 0)
}
