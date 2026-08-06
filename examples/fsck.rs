use std::env;
use std::path::Path;

use git_rs::{FsckOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let report = repository.fsck(&FsckOptions::default())?;
    println!(
        "objects={} reachable={} blobs={} trees={} commits={} tags={}",
        report.objects, report.reachable, report.blobs, report.trees, report.commits, report.tags
    );
    for id in report.dangling() {
        println!("dangling {id}");
    }
    for id in report
        .unreachable()
        .iter()
        .filter(|id| !report.dangling().contains(id))
    {
        println!("unreachable {id}");
    }
    Ok(())
}
