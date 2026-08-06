use std::env;

use git_rs::{HostFileSystem, Repository, Result, ShowRefOptions};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: show_ref <repository> [--head] [-d] [--branches] [--tags] [pattern...]".into(),
        )
    })?;
    let mut options = ShowRefOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--head" => options.include_head = true,
            "-d" | "--dereference" => options.dereference_tags = true,
            "--branches" | "--heads" => options.branches_only = true,
            "--tags" => options.tags_only = true,
            _ => options.patterns.push(argument),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for reference in repository.show_refs(&options)? {
        println!("{} {}", reference.id(), reference.name());
    }
    Ok(())
}
