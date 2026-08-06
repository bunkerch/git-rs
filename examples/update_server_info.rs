use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository, ServerInfoOptions};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let report = repository.update_server_info(&ServerInfoOptions {
        force: arguments.iter().any(|argument| argument == "--force"),
        dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
        ..ServerInfoOptions::default()
    })?;
    println!(
        "refs={} peeled={} packs={} refs-changed={} packs-changed={}",
        report.references,
        report.peeled_tags,
        report.packs,
        report.refs_changed,
        report.packs_changed,
    );
    Ok(())
}
