use std::env;
use std::io::{self, Read};

use git_rs::{HostFileSystem, ObjectKind, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let mut contents = Vec::new();
    io::stdin().read_to_end(&mut contents)?;

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    println!("{}", repository.write_object(ObjectKind::Blob, &contents)?);
    Ok(())
}
