use std::env;
use std::io::{self, Write};
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, PackOptions, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .expect("usage: pack_objects <repository> <object-id>...");
    let ids = arguments
        .map(|value| ObjectId::from_str(&value))
        .collect::<git_rs::Result<Vec<_>>>()?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let bundle = repository.build_pack(&ids, &PackOptions::default())?;
    io::stdout().write_all(bundle.pack())?;
    Ok(())
}
