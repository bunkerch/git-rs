use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let action = arguments.next().unwrap_or_else(|| "show".to_owned());
    let name = arguments.next().expect("worktree administrative name");
    let reason = arguments.next();
    let absolute = std::fs::canonicalize(Path::new(&repository_path))?;
    let virtual_path = absolute
        .strip_prefix("/")
        .expect("absolute path below root");
    let repository = Repository::open(HostFileSystem::new("/")?, virtual_path)?;

    match action.as_str() {
        "lock" => repository.lock_worktree(&name, reason.as_deref())?,
        "unlock" => repository.unlock_worktree(&name)?,
        "show" => println!("{:?}", repository.worktree_lock_reason(&name)?),
        _ => panic!("action must be lock, unlock, or show"),
    }
    Ok(())
}
