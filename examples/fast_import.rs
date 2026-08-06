use std::env;
use std::fs;
use std::io::{self, Write};

use git_rs::{FastImportOptions, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: fast_import REPOSITORY STREAM_FILE [--force] [--require-done]")?;
    let stream_path = arguments
        .next()
        .ok_or("usage: fast_import REPOSITORY STREAM_FILE [--force] [--require-done]")?;
    let mut options = FastImportOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--force" => options.force = true,
            "--require-done" => options.require_done = true,
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let stream = fs::read(stream_path)?;
    let result = repository.fast_import(&stream, &options)?;
    io::stdout().write_all(result.responses())?;
    Ok(())
}
