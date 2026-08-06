use std::{env, path::Path};

use git_rs::{CommitGraphOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let read_only = env::args().any(|argument| argument == "--read");
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    if read_only {
        let graph = repository.read_commit_graph(4 * 1024 * 1024 * 1024usize, 10_000_000)?;
        println!("commits={}", graph.len());
        return Ok(());
    }
    let report = repository.write_commit_graph_reachable(&CommitGraphOptions::default())?;
    println!(
        "commits={} extra-edges={} bytes={} changed={}",
        report.commits, report.extra_edges, report.bytes, report.changed
    );
    Ok(())
}
