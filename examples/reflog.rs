use std::env;

use git_rs::{GraphOptions, HostFileSystem, ReflogRewriteOptions, Repository, Result};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(usage)?;
    let command = arguments.next().ok_or_else(usage)?;
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    if command == "list" {
        for name in repository.reflogs(1_000_000, 4096)? {
            println!("{name}");
        }
        return Ok(());
    }
    let name = arguments.next().ok_or_else(usage)?;
    if command == "show" {
        let entries = repository.read_reflog_bounded(&name, 10_000_000)?;
        for entry in entries {
            println!(
                "{} {} {}\t{}",
                entry.old_id(),
                entry.new_id(),
                entry.committer().encode(),
                String::from_utf8_lossy(entry.message())
            );
        }
        return Ok(());
    }
    if command == "drop" {
        repository.drop_reflog(&name)?;
        return Ok(());
    }
    let value = if command == "stale-fix" {
        0
    } else {
        arguments
            .next()
            .ok_or_else(usage)?
            .parse::<i64>()
            .map_err(|_| git_rs::Error::InvalidRevision("invalid reflog number".into()))?
    };
    let mut options = ReflogRewriteOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--rewrite" => options.rewrite = true,
            "--updateref" => options.update_reference = true,
            "-n" | "--dry-run" => options.dry_run = true,
            _ => {
                return Err(git_rs::Error::InvalidRevision(
                    "unknown reflog option".into(),
                ));
            }
        }
    }
    let result = match command.as_str() {
        "delete" => repository.delete_reflog_entries(
            &name,
            &[usize::try_from(value)
                .map_err(|_| git_rs::Error::InvalidRevision("negative reflog selector".into()))?],
            &options,
        )?,
        "expire" => repository.expire_reflog_before(&name, value, &options)?,
        "expire-unreachable" => repository.expire_reflog_unreachable_before(
            &name,
            value,
            &GraphOptions::default(),
            &options,
        )?,
        "stale-fix" => {
            repository.prune_stale_reflog_entries(&name, &GraphOptions::default(), &options)?
        }
        _ => return Err(usage()),
    };
    println!("{} {}", result.removed, result.retained);
    Ok(())
}

fn usage() -> git_rs::Error {
    git_rs::Error::InvalidRevision(
        "usage: reflog <repository> <list|show|drop|delete|expire|expire-unreachable|stale-fix> [ref] [index|timestamp] [--rewrite] [--updateref] [-n]".into(),
    )
}
