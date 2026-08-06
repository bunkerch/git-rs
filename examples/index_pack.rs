use std::env;
use std::io::{self, Read};

use git_rs::{HostFileSystem, IndexPackOptions, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let repository_path = arguments
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .map_or(".", String::as_str);
    let options = IndexPackOptions {
        strict: arguments.iter().any(|argument| argument == "--strict"),
        dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
        keep_message: arguments
            .iter()
            .find_map(|argument| argument.strip_prefix("--keep=").map(str::as_bytes))
            .map(<[u8]>::to_vec),
        ..IndexPackOptions::default()
    };
    let limit = options.incoming.max_pack_size;
    let mut pack = Vec::new();
    io::stdin()
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut pack)?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let report = repository.index_pack(&pack, &options)?;
    println!(
        "{} objects={} pack-bytes={} index-bytes={} published={}",
        ObjectId::from_bytes(report.checksum),
        report.objects.len(),
        report.pack_size,
        report.index_size,
        report.written.is_some()
    );
    Ok(())
}
