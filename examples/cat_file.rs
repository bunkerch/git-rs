use std::env;
use std::io::{self, Write};
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let object_id = env::args()
        .nth(2)
        .expect("usage: cat_file <repository> <object-id>");
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let object = repository.read_object(ObjectId::from_str(&object_id)?, 1024 * 1024 * 1024)?;
    io::stdout().write_all(object.data())?;
    Ok(())
}
