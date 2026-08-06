use std::env;

use git_rs::{HostFileSystem, Repository, WriteTreeOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: write_tree REPOSITORY [--missing-ok] [--prefix=VALUE]")?;
    let mut options = WriteTreeOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--missing-ok" => options.missing_ok = true,
            _ if argument.starts_with("--prefix=") => {
                options.prefix = Some(argument.as_bytes()["--prefix=".len()..].to_vec());
            }
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    println!("{}", repository.write_current_index_tree(&options)?);
    Ok(())
}
