use std::env;
use std::str::FromStr;

use git_rs::{CheckoutOptions, HostFileSystem, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let tree = env::args()
        .nth(2)
        .expect("usage: checkout_tree <repository> <tree-id> [--force]");
    let force = env::args().any(|argument| argument == "--force");
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let written = repository.checkout_tree(
        ObjectId::from_str(&tree)?,
        &CheckoutOptions {
            force,
            ..CheckoutOptions::default()
        },
    )?;
    println!("updated {written} worktree entries");
    Ok(())
}
