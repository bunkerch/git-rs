use std::env;
use std::str::FromStr;

use git_rs::{ForkPointOptions, GraphOptions, HostFileSystem, ObjectId, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: merge_base REPOSITORY [--octopus|--independent|--is-ancestor|--fork-point REF] COMMIT...",
    )?;
    let arguments = arguments.collect::<Vec<_>>();
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let graph = GraphOptions::default();

    match arguments.first().map(String::as_str) {
        Some("--octopus") => {
            print_ids(repository.octopus_merge_bases(&parse_ids(&arguments[1..])?, &graph)?);
        }
        Some("--independent") => {
            print_ids(repository.independent_commits(&parse_ids(&arguments[1..])?, &graph)?);
        }
        Some("--is-ancestor") => {
            let ids = parse_ids(&arguments[1..])?;
            if ids.len() != 2 {
                return Err("--is-ancestor requires exactly two commits".into());
            }
            println!("{}", repository.is_ancestor(ids[0], ids[1], &graph)?);
        }
        Some("--fork-point") => {
            if arguments.len() != 3 {
                return Err("--fork-point requires a ref and a commit".into());
            }
            let derived = ObjectId::from_str(&arguments[2])?;
            if let Some(id) =
                repository.fork_point(&arguments[1], derived, &ForkPointOptions::default())?
            {
                println!("{id}");
            }
        }
        _ => {
            let ids = parse_ids(&arguments)?;
            let (one, others) = ids
                .split_first()
                .ok_or("merge-base requires at least two commits")?;
            print_ids(repository.merge_bases_many(*one, others, &graph)?);
        }
    }
    Ok(())
}

fn parse_ids(arguments: &[String]) -> Result<Vec<ObjectId>, Box<dyn std::error::Error>> {
    arguments
        .iter()
        .map(|argument| ObjectId::from_str(argument).map_err(Into::into))
        .collect()
}

fn print_ids(ids: Vec<ObjectId>) {
    for id in ids {
        println!("{id}");
    }
}
