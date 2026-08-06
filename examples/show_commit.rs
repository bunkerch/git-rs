use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let object_id = env::args()
        .nth(2)
        .expect("usage: show_commit <repository> <commit-id>");
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let commit = repository.read_commit(ObjectId::from_str(&object_id)?, 16 * 1024 * 1024)?;
    let tree = repository.read_tree(commit.tree(), 64 * 1024 * 1024)?;
    println!("tree {} ({} entries)", commit.tree(), tree.entries().len());
    println!(
        "author {} <{}>",
        commit.author().name(),
        commit.author().email()
    );
    println!("parents {}", commit.parents().len());
    Ok(())
}
