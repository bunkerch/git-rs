use std::env;

use git_rs::{HostFileSystem, PrunePackedOptions, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let write = arguments.any(|argument| argument == "--write");
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    let report = repository.prune_packed(&PrunePackedOptions {
        dry_run: !write,
        ..PrunePackedOptions::default()
    })?;
    for (id, path) in &report.removed {
        println!("{id}\t{}", path.display());
    }
    println!(
        "duplicates={} bytes={} packed={} scanned={}",
        report.removed.len(),
        report.removed_bytes,
        report.packed_objects,
        report.scanned_loose_entries
    );
    Ok(())
}
