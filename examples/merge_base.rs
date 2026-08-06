use std::env;

use git_rs::{GraphOptions, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: merge_base <repository> <one> <two>")?;
    let one = arguments
        .next()
        .ok_or("usage: merge_base <repository> <one> <two>")?;
    let two = arguments
        .next()
        .ok_or("usage: merge_base <repository> <one> <two>")?;
    if arguments.next().is_some() {
        return Err("usage: merge_base <repository> <one> <two>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let one = repository.resolve_reference(&one)?;
    let two = repository.resolve_reference(&two)?;
    for base in repository.merge_bases(one, two, &GraphOptions::default())? {
        println!("{base}");
    }
    Ok(())
}
