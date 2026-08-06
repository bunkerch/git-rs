use std::{env, path::Path};

use git_rs::{HostFileSystem, MaintenanceOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let apply = env::args().any(|argument| argument == "--apply");
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let report = repository.run_maintenance(&MaintenanceOptions {
        dry_run: !apply,
        force: apply,
        ..MaintenanceOptions::full()
    })?;
    println!(
        "dry-run={} tasks={}",
        report.dry_run,
        report.outcomes().len()
    );
    for outcome in report.outcomes() {
        println!("{outcome:?}");
    }
    Ok(())
}
