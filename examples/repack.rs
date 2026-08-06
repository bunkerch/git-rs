use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, RepackOptions, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path must have a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path must name a directory");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let cruft = arguments.iter().any(|argument| argument == "--cruft");
    let cruft_expire_before = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--cruft-expire-before="))
        .map(str::parse::<u64>)
        .transpose()
        .expect("cruft expiry must be Unix seconds");
    let result = repository.repack(&RepackOptions {
        include_unreachable: arguments.iter().any(|argument| argument == "--all-objects"),
        cruft,
        cruft_expire_before,
        prune_loose: arguments.iter().any(|argument| argument == "--prune-loose"),
        delete_redundant_packs: arguments
            .iter()
            .any(|argument| argument == "--delete-redundant-packs"),
        dry_run: arguments.iter().any(|argument| argument == "--dry-run"),
        ..RepackOptions::default()
    })?;
    println!(
        "packed={} pruned-loose={} removed-packs={}",
        result.packed_objects, result.pruned_loose_objects, result.removed_packs
    );
    if let Some(pack) = result.pack {
        println!("pack={}", pack.pack_path.display());
        println!("index={}", pack.index_path.display());
    }
    if let Some(pack) = result.cruft_pack {
        println!("cruft-pack={}", pack.pack_path.display());
        println!("cruft-index={}", pack.index_path.display());
        println!(
            "cruft-mtimes={}",
            pack.pack_path.with_extension("mtimes").display()
        );
    }
    Ok(())
}
