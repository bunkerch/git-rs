use std::{env, path::Path};

use git_rs::{ApplyOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let patch_path = env::args()
        .nth(2)
        .expect("usage: apply REPOSITORY PATCH [--check] [--index] [--reverse]");
    let arguments = env::args().skip(3).collect::<Vec<_>>();
    let patch = std::fs::read(patch_path)?;
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let report = repository.apply_patch(
        &patch,
        &ApplyOptions {
            check: arguments.iter().any(|argument| argument == "--check"),
            index: arguments.iter().any(|argument| argument == "--index"),
            reverse: arguments.iter().any(|argument| argument == "--reverse"),
            ..ApplyOptions::default()
        },
    )?;
    println!(
        "files={} hunks={} created={} modified={} deleted={}",
        report.files, report.hunks, report.created, report.modified, report.deleted
    );
    Ok(())
}
