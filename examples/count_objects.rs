use std::env;

use git_rs::{CountObjectsOptions, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repository_path = env::args()
        .nth(1)
        .ok_or("usage: count_objects <repository>")?;
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let report = repository.count_objects(&CountObjectsOptions::default())?;
    println!("count: {}", report.loose_objects);
    println!("size: {}", report.loose_bytes / 1024);
    println!("in-pack: {}", report.packed_objects);
    println!("packs: {}", report.packs);
    println!("size-pack: {}", report.packed_bytes / 1024);
    println!("prune-packable: {}", report.prune_packable);
    println!("garbage: {}", report.garbage.len());
    println!("size-garbage: {}", report.garbage_bytes / 1024);
    Ok(())
}
