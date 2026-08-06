use std::env;
use std::io::{self, Read};

use git_rs::{HashObjectOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let repository_path = arguments
        .iter()
        .find(|argument| argument.as_str() != "--write")
        .cloned()
        .unwrap_or_else(|| ".".to_owned());
    let mut contents = Vec::new();
    io::stdin().read_to_end(&mut contents)?;

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let write = arguments.iter().any(|argument| argument == "--write");
    println!(
        "{}",
        repository.hash_object(
            &contents,
            &HashObjectOptions {
                write,
                ..HashObjectOptions::default()
            }
        )?
    );
    Ok(())
}
