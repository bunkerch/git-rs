use std::env;

use git_rs::{ChangeKind, HostFileSystem, Repository, StatusOptions};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    for entry in repository.status(&StatusOptions::default())?.entries() {
        if entry.worktree_change() == Some(ChangeKind::Untracked) {
            println!("?? {}", String::from_utf8_lossy(entry.path()));
        } else {
            println!(
                "{}{} {}",
                code(entry.index_change()),
                code(entry.worktree_change()),
                String::from_utf8_lossy(entry.path())
            );
        }
    }
    Ok(())
}

const fn code(change: Option<ChangeKind>) -> char {
    match change {
        None => ' ',
        Some(ChangeKind::Added) => 'A',
        Some(ChangeKind::Modified) => 'M',
        Some(ChangeKind::Deleted) => 'D',
        Some(ChangeKind::TypeChanged) => 'T',
        Some(ChangeKind::Unmerged) => 'U',
        Some(ChangeKind::Untracked) => '?',
    }
}
