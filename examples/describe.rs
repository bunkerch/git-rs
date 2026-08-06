use std::env;

use git_rs::{DescribeOptions, HostFileSystem, Repository, Result};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(usage)?;
    let mut target = String::from("HEAD");
    let mut options = DescribeOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--all" => options.all = true,
            "--tags" => options.tags = true,
            "--long" => options.long = true,
            "--first-parent" => options.first_parent = true,
            "--always" => options.always = true,
            "--exact-match" => options.exact_match = true,
            _ if argument.starts_with("--abbrev=") => {
                options.abbreviation = argument[9..]
                    .parse()
                    .map_err(|_| git_rs::Error::InvalidRevision("invalid abbreviation".into()))?;
            }
            _ if argument.starts_with("--candidates=") => {
                options.max_candidates = argument[13..].parse().map_err(|_| {
                    git_rs::Error::InvalidRevision("invalid candidate count".into())
                })?;
            }
            _ if argument.starts_with("--match=") => {
                options
                    .match_patterns
                    .push(argument.as_bytes()[8..].to_vec());
            }
            _ if argument.starts_with("--exclude=") => {
                options
                    .exclude_patterns
                    .push(argument.as_bytes()[10..].to_vec());
            }
            _ if target == "HEAD" => target = argument,
            _ => return Err(usage()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    println!("{}", repository.describe(&target, &options)?.rendered());
    Ok(())
}

fn usage() -> git_rs::Error {
    git_rs::Error::InvalidRevision(
        "usage: describe <repository> [commit-ish] [--all] [--tags] [--long] [--first-parent] [--always] [--exact-match] [--abbrev=N] [--candidates=N] [--match=PATTERN] [--exclude=PATTERN]".into(),
    )
}
