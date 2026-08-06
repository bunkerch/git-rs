use std::env;
use std::io::{self, Read};

use git_rs::{HostFileSystem, MkTagOptions, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let mut options = MkTagOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--no-strict" => options.strict = false,
            "--dry-run" => options.dry_run = true,
            value => {
                return Err(git_rs::Error::InvalidRepository(format!(
                    "unsupported mktag option `{value}`"
                )));
            }
        }
    }
    let limit = u64::try_from(options.max_input_size)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut input = Vec::new();
    io::stdin().take(limit).read_to_end(&mut input)?;
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    println!("{}", repository.mk_tag(&input, &options)?);
    Ok(())
}
