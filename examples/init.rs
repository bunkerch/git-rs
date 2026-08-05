use std::env;

use git_rs::{HostFileSystem, InitOptions, Repository};

fn main() -> git_rs::Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "example-repo".to_owned());
    let storage = HostFileSystem::new(".")?;
    let repository = Repository::init(storage, &path, &InitOptions::default())?;
    println!("initialized {}", repository.git_dir().display());
    Ok(())
}
