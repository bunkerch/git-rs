use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository, UnpackFileOptions};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let id = ObjectId::from_str(
        &env::args()
            .nth(2)
            .expect("usage: unpack_file <repository> <blob-id>"),
    )?;
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    let file = repository.unpack_file(id, &UnpackFileOptions::default())?;
    println!("{}", file.path().display());
    Ok(())
}
