use std::env;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{
    HostFileSystem, Repository, Signature, StashApplyOptions, StashApplyResult, StashPushOptions,
};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let operation = env::args().nth(2).unwrap_or_else(|| "list".to_owned());
    let arguments = env::args().skip(3).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;

    match operation.as_str() {
        "push" => {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_secs()
                .try_into()
                .expect("current timestamp must fit in i64");
            let name = env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "git-rs".to_owned());
            let email =
                env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "git-rs@example.com".to_owned());
            let committer = Signature::new(name, email, timestamp, 0)?;
            let include_untracked = arguments.iter().any(|value| value == "--include-untracked");
            let message = arguments
                .iter()
                .position(|value| value == "--message")
                .and_then(|position| arguments.get(position + 1))
                .map(|value| value.as_bytes().to_vec());
            let id = repository.stash_push(
                &StashPushOptions {
                    include_untracked,
                    message,
                    ..StashPushOptions::default()
                },
                &committer,
            )?;
            println!("saved {id}");
        }
        "apply" | "pop" => {
            let index = parse_index(arguments.first())?;
            let options = StashApplyOptions {
                reinstate_index: arguments.iter().any(|value| value == "--index"),
                ..StashApplyOptions::default()
            };
            let result = if operation == "apply" {
                repository.stash_apply(index, &options)?
            } else {
                repository.stash_pop(index, &options)?
            };
            match result {
                StashApplyResult::Applied => println!("applied stash@{{{index}}}"),
                StashApplyResult::Conflicted { paths } => {
                    println!("conflicts in {} path(s)", paths.len());
                }
            }
        }
        "drop" => {
            let index = parse_index(arguments.first())?;
            println!("dropped {}", repository.stash_drop(index)?);
        }
        "list" => {
            for entry in repository.stashes()? {
                println!(
                    "stash@{{{}}}: {}",
                    entry.index(),
                    String::from_utf8_lossy(entry.message())
                );
            }
        }
        other => panic!("unknown stash operation: {other}"),
    }
    Ok(())
}

fn parse_index(value: Option<&String>) -> git_rs::Result<usize> {
    match value {
        None => Ok(0),
        Some(value) if value.starts_with("--") => Ok(0),
        Some(value) => value
            .parse()
            .map_err(|_| git_rs::Error::InvalidReference("invalid stash index".into())),
    }
}
