use std::env;

use git_rs::{DiffOptions, HostFileSystem, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: patch_id <repository> <commit>")?;
    let expression = arguments
        .next()
        .ok_or("usage: patch_id <repository> <commit>")?;
    if arguments.next().is_some() {
        return Err("usage: patch_id <repository> <commit>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let commit = repository.resolve_revision_id(&expression, &RevisionOptions::default())?;
    match repository.commit_patch_id(commit, &DiffOptions::default())? {
        Some(id) => println!("{id}"),
        None => println!("merge commits do not have a patch identity"),
    }
    Ok(())
}
