use std::env;
use std::io::{self, Read, Write};

use git_rs::{CatFileBatchOptions, CatFileMode, HostFileSystem, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: cat_file REPOSITORY [-p|-t|-s|-e|--batch|--batch-check] [-Z] [OBJECT]")?;
    let mut mode = CatFileMode::Content;
    let mut batch = None;
    let mut nul_terminated = false;
    let mut object = None;
    for argument in arguments {
        match argument.as_str() {
            "-p" => mode = CatFileMode::Pretty,
            "-t" => mode = CatFileMode::Type,
            "-s" => mode = CatFileMode::Size,
            "-e" => mode = CatFileMode::Exists,
            "--batch" => batch = Some(true),
            "--batch-check" => batch = Some(false),
            "-Z" => nul_terminated = true,
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option: {argument}").into());
            }
            _ if object.is_none() => object = Some(argument),
            _ => return Err("multiple object arguments".into()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    if let Some(contents) = batch {
        if object.is_some() {
            return Err("batch mode does not take an object argument".into());
        }
        let options = CatFileBatchOptions {
            contents,
            nul_terminated,
            ..CatFileBatchOptions::default()
        };
        let mut input = Vec::new();
        io::stdin()
            .take(options.max_input_bytes as u64 + 1)
            .read_to_end(&mut input)?;
        io::stdout().write_all(&repository.cat_file_batch(&input, &options)?)?;
        return Ok(());
    }
    let expression = object.ok_or("missing object argument")?;
    let id = repository.resolve_revision_id(&expression, &RevisionOptions::default())?;
    let result = repository.cat_file(id, mode, None, 1024 * 1024 * 1024, 1024 * 1024 * 1024)?;
    if mode == CatFileMode::Exists && !result.exists {
        return Err("object does not exist".into());
    }
    io::stdout().write_all(&result.output)?;
    Ok(())
}
