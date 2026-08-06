use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: check-ignore <repository> <path> [--directory]")?;
    let path = arguments
        .next()
        .ok_or("usage: check-ignore <repository> <path> [--directory]")?;
    let directory = match arguments.next().as_deref() {
        None => false,
        Some("--directory") => true,
        Some(_) => return Err("usage: check-ignore <repository> <path> [--directory]".into()),
    };
    if arguments.next().is_some() {
        return Err("usage: check-ignore <repository> <path> [--directory]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    println!("{}", repository.is_ignored(Path::new(&path), directory)?);
    Ok(())
}
