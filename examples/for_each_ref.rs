use std::env;

use git_rs::{ForEachRefOptions, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).unwrap_or_else(|| ".".into());
    let patterns = env::args().skip(2).collect();
    let repository = Repository::open(HostFileSystem::new(path)?, ".")?;
    for entry in repository.for_each_ref(&ForEachRefOptions {
        patterns,
        ..ForEachRefOptions::default()
    })? {
        println!(
            "{} {} {}",
            match entry.object_kind() {
                git_rs::ObjectKind::Blob => "blob",
                git_rs::ObjectKind::Commit => "commit",
                git_rs::ObjectKind::Tag => "tag",
                git_rs::ObjectKind::Tree => "tree",
            },
            entry.object_id(),
            entry.name()
        );
    }
    Ok(())
}
