use std::env;

use git_rs::{HostFileSystem, Repository, RevisionWalkOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let revision = arguments.next().unwrap_or_else(|| "HEAD".to_owned());
    if arguments.next().is_some() {
        return Err("usage: log [repository] [revision]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let tip = repository.resolve_reference(&revision)?;
    for entry in repository.walk_revisions(&[tip], &[], &RevisionWalkOptions::default())? {
        let subject = entry
            .commit()
            .message()
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        println!("{} {}", entry.id(), String::from_utf8_lossy(subject));
    }
    Ok(())
}
