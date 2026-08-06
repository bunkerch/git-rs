use std::env;

use git_rs::{HostFileSystem, MoveWorktreeOptions, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let name = arguments.next().expect("worktree administrative name");
    let destination = arguments.next().expect("destination path");
    let override_locks = arguments.any(|argument| argument == "--override-locks");

    let repository_path = std::fs::canonicalize(repository_path)?;
    let repository = Repository::open(
        HostFileSystem::new("/")?,
        repository_path
            .strip_prefix("/")
            .expect("absolute repository path"),
    )?;
    let destination = std::path::absolute(destination)?;
    let moved = repository.move_worktree(
        &name,
        destination
            .strip_prefix("/")
            .expect("absolute destination path"),
        &MoveWorktreeOptions {
            override_locks,
            ..Default::default()
        },
    )?;
    println!("/{}", moved.display());
    Ok(())
}
