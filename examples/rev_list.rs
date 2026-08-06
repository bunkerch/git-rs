use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository, RevListOptions, RevListOrder, RevListSide};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: rev_list REPOSITORY [--symmetric] [--boundary] [--reverse] [--objects] REV...",
    )?;
    let mut symmetric = false;
    let mut options = RevListOptions::default();
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for argument in arguments {
        match argument.as_str() {
            "--symmetric" => symmetric = true,
            "--boundary" => options.boundary = true,
            "--reverse" => options.order = RevListOrder::Reverse,
            "--objects" => options.objects = true,
            _ if argument.starts_with('^') => {
                exclude.push(ObjectId::from_str(&argument[1..])?);
            }
            _ => include.push(ObjectId::from_str(&argument)?),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let result = if symmetric {
        if include.len() != 2 || !exclude.is_empty() {
            return Err("--symmetric requires exactly two non-excluded commits".into());
        }
        repository.rev_list_symmetric(include[0], include[1], &options)?
    } else {
        repository.rev_list(&include, &exclude, &options)?
    };
    for entry in result.commits() {
        let marker = match entry.side() {
            RevListSide::Unmarked => "",
            RevListSide::Left => "<",
            RevListSide::Right => ">",
            RevListSide::Boundary => "-",
        };
        println!("{marker}{}", entry.id());
    }
    for object in result.objects() {
        println!("{} {}", object.id(), String::from_utf8_lossy(object.path()));
    }
    Ok(())
}
