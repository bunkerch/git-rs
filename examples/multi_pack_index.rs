use std::{env, path::Path};

use git_rs::{HostFileSystem, MultiPackIndexOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let read_only = env::args().any(|argument| argument == "--read");
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    if read_only {
        let midx = repository.read_multi_pack_index(
            8 * 1024 * 1024 * 1024usize,
            1_000_000,
            100_000_000,
        )?;
        println!("packs={} objects={}", midx.pack_names().len(), midx.len());
        return Ok(());
    }
    let report = repository.write_multi_pack_index(&MultiPackIndexOptions::default())?;
    println!(
        "packs={} objects={} duplicates={} bytes={} changed={}",
        report.packs, report.objects, report.duplicate_objects, report.bytes, report.changed
    );
    Ok(())
}
