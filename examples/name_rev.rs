use std::env;
use std::str::FromStr;

use git_rs::{HostFileSystem, NameRevOptions, ObjectId, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: name_rev REPOSITORY [--tags] [--always] [--no-undefined] OBJECT...")?;
    let mut options = NameRevOptions::default();
    let mut targets = Vec::new();
    for argument in arguments {
        match argument.as_str() {
            "--tags" => {
                options.tags_only = true;
                options.shorten_tags = true;
            }
            "--always" => options.always = true,
            "--no-undefined" => options.allow_undefined = false,
            _ => targets.push(ObjectId::from_str(&argument)?),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for entry in repository.name_revs(&targets, &options)? {
        println!("{}", entry.name().unwrap_or("undefined"));
    }
    Ok(())
}
