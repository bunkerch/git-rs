use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, ObjectId, ReadTreeOptions, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: read_tree REPOSITORY [--empty|--merge|--reset|--prefix=VALUE] TREE...")?;
    let mut options = ReadTreeOptions::default();
    let mut trees = Vec::new();
    for argument in arguments {
        match argument.as_str() {
            "--empty" => options.empty = true,
            "--merge" => options.merge = true,
            "--reset" => options.reset = true,
            "--dry-run" => options.dry_run = true,
            "--aggressive" => options.aggressive = true,
            _ if argument.starts_with("--prefix=") => {
                options.prefix = Some(argument.as_bytes()["--prefix=".len()..].to_vec());
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option: {argument}").into());
            }
            _ => trees.push(ObjectId::from_str(&argument)?),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let result = repository.read_tree_into_index(&trees, &options)?;
    for path in result.conflicts {
        println!("conflict {}", String::from_utf8_lossy(&path));
    }
    Ok(())
}
