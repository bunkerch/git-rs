use std::env;
use std::fs;

use git_rs::{FastExportOptions, HostFileSystem, ReferenceName, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: fast_export REPOSITORY OUTPUT REF...")?;
    let output_path = arguments
        .next()
        .ok_or("usage: fast_export REPOSITORY OUTPUT REF...")?;
    let references = arguments
        .map(ReferenceName::new)
        .collect::<Result<Vec<_>, _>>()?;
    if references.is_empty() {
        return Err("at least one fully-qualified ref is required".into());
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let result = repository.fast_export(&references, &FastExportOptions::default())?;
    fs::write(output_path, result.stream())?;
    Ok(())
}
