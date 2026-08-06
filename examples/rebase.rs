use std::env;

use git_rs::{HostFileSystem, RebaseOptions, Repository, Signature};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: rebase <repository> <upstream-ref> [onto-ref]")?;
    let upstream_name = arguments
        .next()
        .ok_or("usage: rebase <repository> <upstream-ref> [onto-ref]")?;
    let onto_name = arguments.next().unwrap_or_else(|| upstream_name.clone());
    if arguments.next().is_some() {
        return Err("usage: rebase <repository> <upstream-ref> [onto-ref]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let upstream = repository.resolve_reference(&upstream_name)?;
    let onto = repository.resolve_reference(&onto_name)?;
    let committer = Signature::new("git-rs", "git-rs@example.com", 0, 0)?;
    println!(
        "{:?}",
        repository.rebase(upstream, onto, &RebaseOptions::default(), &committer)?
    );
    Ok(())
}
