use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository, SubmoduleOptions};

fn main() -> git_rs::Result<()> {
    let path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let path = std::fs::canonicalize(Path::new(&path))?;
    let repository = Repository::open(
        HostFileSystem::new(path.parent().expect("repository parent"))?,
        path.file_name().expect("repository name"),
    )?;
    for status in repository.submodule_status(&SubmoduleOptions::default())? {
        let id = status.expected().map_or_else(
            || "0000000000000000000000000000000000000000".to_owned(),
            |id| id.to_string(),
        );
        println!(
            "{}{id} {}",
            status.prefix(),
            String::from_utf8_lossy(status.module().path())
        );
    }
    Ok(())
}
