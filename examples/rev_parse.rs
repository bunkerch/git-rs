use std::env;

use git_rs::{HostFileSystem, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: rev-parse <repository> <expression>")?;
    let expression = arguments
        .next()
        .ok_or("usage: rev-parse <repository> <expression>")?;
    if arguments.next().is_some() {
        return Err("usage: rev-parse <repository> <expression>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let resolved = repository.resolve_revision(&expression, &RevisionOptions::default())?;
    println!("{} {:?}", resolved.id, resolved.kind);
    Ok(())
}
