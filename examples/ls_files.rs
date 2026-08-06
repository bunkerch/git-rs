use std::env;

use git_rs::{HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let index = repository.read_index()?;
    println!("version {}", index.version() as u32);
    for entry in index.entries() {
        println!(
            "{:06o} {} {}\t{}",
            entry.mode(),
            entry.id(),
            entry.stage(),
            String::from_utf8_lossy(entry.path())
        );
    }
    Ok(())
}
