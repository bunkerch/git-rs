use std::env;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{HostFileSystem, ObjectId, Repository, ResetMode, ResetOptions, Signature};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let target = env::args()
        .nth(2)
        .expect("usage: reset <repository> <commit> [soft|mixed|hard]");
    let mode = match env::args().nth(3).as_deref() {
        None | Some("mixed") => ResetMode::Mixed,
        Some("soft") => ResetMode::Soft,
        Some("hard") => ResetMode::Hard,
        Some(value) => {
            return Err(git_rs::Error::InvalidRepository(format!(
                "unknown reset mode {value}"
            )));
        }
    };
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    repository.reset(
        ObjectId::from_str(&target)?,
        &ResetOptions {
            mode,
            ..ResetOptions::default()
        },
        &identity()?,
    )
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
