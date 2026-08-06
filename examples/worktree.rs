use std::env;

use git_rs::{AddWorktreeOptions, HostFileSystem, Repository, WorktreeTarget};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: worktree <repository> <path> <name> <branch>")?;
    let path = arguments
        .next()
        .ok_or("usage: worktree <repository> <path> <name> <branch>")?;
    let name = arguments
        .next()
        .ok_or("usage: worktree <repository> <path> <name> <branch>")?;
    let branch = arguments
        .next()
        .ok_or("usage: worktree <repository> <path> <name> <branch>")?;
    if arguments.next().is_some() {
        return Err("usage: worktree <repository> <path> <name> <branch>".into());
    }

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let linked = repository.add_worktree(
        path,
        &name,
        &WorktreeTarget::Branch(branch),
        &AddWorktreeOptions::default(),
    )?;
    println!(
        "created {}",
        linked.work_tree().expect("linked worktree").display()
    );
    Ok(())
}
