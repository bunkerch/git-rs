use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let paths = env::args().skip(2).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
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
