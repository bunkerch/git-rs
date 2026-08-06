use std::env;
use std::path::Path;

use git_rs::{ArchiveFormat, ArchiveOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    assert!(
        arguments.len() >= 3,
        "usage: archive <repository> <revision> <output> [--zip] [--worktree-attributes] [--prefix=<path>] [path...]"
    );
    let repository_path = std::fs::canonicalize(Path::new(&arguments[0]))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let format = if arguments.iter().any(|argument| argument == "--zip") {
        ArchiveFormat::Zip
    } else {
        ArchiveFormat::Tar
    };
    let prefix = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--prefix="))
        .unwrap_or("")
        .as_bytes()
        .to_vec();
    let paths = arguments[3..]
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .map(|argument| argument.as_bytes().to_vec())
        .collect();
    let bytes = repository.archive(
        &arguments[1],
        &ArchiveOptions {
            format,
            prefix,
            paths,
            worktree_attributes: arguments
                .iter()
                .any(|argument| argument == "--worktree-attributes"),
            ..ArchiveOptions::default()
        },
    )?;
    std::fs::write(&arguments[2], bytes)?;
    Ok(())
}
