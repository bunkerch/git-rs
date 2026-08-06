use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: replace <repository> <original-id> <replacement-id>")?;
    let original = ObjectId::from_str(
        &arguments
            .next()
            .ok_or("usage: replace <repository> <original-id> <replacement-id>")?,
    )?;
    let replacement = ObjectId::from_str(
        &arguments
            .next()
            .ok_or("usage: replace <repository> <original-id> <replacement-id>")?,
    )?;
    if arguments.next().is_some() {
        return Err("usage: replace <repository> <original-id> <replacement-id>".into());
    }

    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let replacement = repository.create_replacement(original, replacement, false, 1 << 30)?;
    println!(
        "{} -> {}",
        replacement.original(),
        replacement.replacement()
    );
    Ok(())
}
