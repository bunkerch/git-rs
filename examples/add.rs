use std::env;

use git_rs::{HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let paths = env::args().skip(2).collect::<Vec<_>>();
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    if paths.is_empty() {
        repository.add(".")?;
    } else {
        for path in paths {
            repository.add(path)?;
        }
    }
    let tree = repository.write_index_tree(&repository.read_index()?)?;
    println!("{tree}");
    Ok(())
}
