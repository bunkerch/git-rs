use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let branch = env::args()
        .nth(2)
        .unwrap_or_else(|| "new-branch".to_owned());
    let target = env::args()
        .nth(3)
        .expect("usage: branch <repository> <branch> <object-id>");

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let reference = repository.create_branch(&branch, ObjectId::from_str(&target)?, false)?;
    println!("created {}", reference.name());
    Ok(())
}
