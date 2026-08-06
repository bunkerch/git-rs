use std::env;

use git_rs::{CherryOptions, HostFileSystem, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: cherry <repository> <upstream> <head> [limit]")?;
    let upstream = arguments
        .next()
        .ok_or("usage: cherry <repository> <upstream> <head> [limit]")?;
    let head = arguments
        .next()
        .ok_or("usage: cherry <repository> <upstream> <head> [limit]")?;
    let limit = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: cherry <repository> <upstream> <head> [limit]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let revisions = RevisionOptions::default();
    let upstream = repository.resolve_revision_id(&upstream, &revisions)?;
    let head = repository.resolve_revision_id(&head, &revisions)?;
    let limit = limit
        .map(|limit| repository.resolve_revision_id(&limit, &revisions))
        .transpose()?;
    for commit in repository.cherry(upstream, head, limit, &CherryOptions::default())? {
        println!("{} {}", commit.marker(), commit.id());
    }
    Ok(())
}
