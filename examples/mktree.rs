use std::env;
use std::io::{self, Read};

use git_rs::{HostFileSystem, MkTreeOptions, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: mktree REPOSITORY [-z] [--missing] [--batch]")?;
    let mut options = MkTreeOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "-z" => options.nul_terminated = true,
            "--missing" => options.missing = true,
            "--batch" => options.batch = true,
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    let mut input = Vec::new();
    io::stdin()
        .take(options.max_input_bytes as u64 + 1)
        .read_to_end(&mut input)?;
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for id in repository.mk_tree(&input, &options)? {
        println!("{id}");
    }
    Ok(())
}
