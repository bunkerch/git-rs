use std::env;
use std::io::{self, Read};

use git_rs::{HostFileSystem, Repository, UnpackObjectsOptions};

fn main() -> git_rs::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let repository_path = arguments
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .map_or(".", String::as_str);
    let options = UnpackObjectsOptions {
        dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
        strict: arguments.iter().any(|argument| argument == "--strict"),
        ..UnpackObjectsOptions::default()
    };
    let input_limit = options.incoming.max_pack_size;
    let mut pack = Vec::new();
    io::stdin()
        .take(
            u64::try_from(input_limit)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut pack)?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let report = repository.unpack_objects(&pack, &options)?;
    println!(
        "objects={} written={} existing={}",
        report.objects.len(),
        report.written.len(),
        report.existing.len()
    );
    Ok(())
}
