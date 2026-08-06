use std::{env, path::Path};

use git_rs::{CommitGraphOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let tip = repository.resolve_reference("HEAD")?;
    let report = repository.write_commit_graph(&[tip], &CommitGraphOptions::default())?;
    println!(
        "commits={} extra-edges={} bytes={} changed={}",
        report.commits, report.extra_edges, report.bytes, report.changed
    );
    Ok(())
}
