use std::env;

use git_rs::{HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let name = arguments.next().expect("worktree administrative name");
    let worktree_path = arguments.next().expect("new worktree path");

    let repository_path = std::fs::canonicalize(repository_path)?;
    let worktree_path = std::fs::canonicalize(worktree_path)?;
    let repository = Repository::open(
        HostFileSystem::new("/")?,
        repository_path
            .strip_prefix("/")
            .expect("absolute repository path"),
    )?;
    let changed = repository.repair_worktree(
        &name,
        worktree_path
            .strip_prefix("/")
            .expect("absolute worktree path"),
    )?;
    println!("{}", if changed { "repaired" } else { "already valid" });
    Ok(())
}
