use std::env;

use git_rs::{HostFileSystem, MergeOptions, Repository, Signature};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: merge <repository> <target-ref>")?;
    let target = arguments
        .next()
        .ok_or("usage: merge <repository> <target-ref>")?;
    if arguments.next().is_some() {
        return Err("usage: merge <repository> <target-ref>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let target = repository.resolve_reference(&target)?;
    let signature = Signature::new("git-rs", "git-rs@example.com", 0, 0)?;
    let result = repository.merge(target, &MergeOptions::default(), &signature)?;
    println!("{result:?}");
    Ok(())
}
