use std::env;

use git_rs::{HostFileSystem, ReplayKind, ReplayOptions, Repository, Signature};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: replay <repository> <cherry-pick|revert> <commit-ref>")?;
    let kind = match arguments
        .next()
        .ok_or("usage: replay <repository> <cherry-pick|revert> <commit-ref>")?
        .as_str()
    {
        "cherry-pick" => ReplayKind::CherryPick,
        "revert" => ReplayKind::Revert,
        _ => return Err("kind must be `cherry-pick` or `revert`".into()),
    };
    let target = arguments
        .next()
        .ok_or("usage: replay <repository> <cherry-pick|revert> <commit-ref>")?;
    if arguments.next().is_some() {
        return Err("usage: replay <repository> <cherry-pick|revert> <commit-ref>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let target = repository.resolve_reference(&target)?;
    let committer = Signature::new("git-rs", "git-rs@example.com", 0, 0)?;
    let result = repository.replay_commit(target, kind, &ReplayOptions::default(), &committer)?;
    println!("{result:?}");
    Ok(())
}
