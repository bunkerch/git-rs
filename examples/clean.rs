use std::env;
use std::path::{Path, PathBuf};

use git_rs::{CleanIgnoredMode, CleanOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let force = arguments.iter().any(|argument| argument == "--force");
    let paths = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let ignored = if arguments
        .iter()
        .any(|argument| argument == "--ignored-only")
    {
        CleanIgnoredMode::Only
    } else if arguments
        .iter()
        .any(|argument| argument == "--include-ignored")
    {
        CleanIgnoredMode::Include
    } else {
        CleanIgnoredMode::Respect
    };

    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let entries = repository.clean(
        &paths,
        &CleanOptions {
            directories: arguments.iter().any(|argument| argument == "--directories"),
            ignored,
            force,
            remove_nested_repositories: arguments
                .iter()
                .any(|argument| argument == "--remove-nested-repositories"),
            dry_run: !force,
            ..CleanOptions::default()
        },
    )?;
    for entry in entries {
        let action = if force { "removed" } else { "would remove" };
        let suffix = if entry.is_directory() { "/" } else { "" };
        println!(
            "{action} '{}{suffix}'",
            String::from_utf8_lossy(entry.path())
        );
    }
    Ok(())
}
