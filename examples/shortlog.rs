use std::env;

use git_rs::{HostFileSystem, Repository, RevisionOptions, ShortlogOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: shortlog <repository> <head> [exclude]")?;
    let head = arguments
        .next()
        .ok_or("usage: shortlog <repository> <head> [exclude]")?;
    let exclude = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: shortlog <repository> <head> [exclude]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let revisions = RevisionOptions::default();
    let head = repository.resolve_revision_id(&head, &revisions)?;
    let exclude = exclude
        .map(|exclude| repository.resolve_revision_id(&exclude, &revisions))
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    for entry in repository.shortlog(
        &[head],
        &exclude,
        &ShortlogOptions {
            include_email: true,
            sort_by_number: true,
            ..ShortlogOptions::default()
        },
    )? {
        println!(
            "{:6}\t{}",
            entry.commit_count(),
            String::from_utf8_lossy(entry.identity())
        );
    }
    Ok(())
}
