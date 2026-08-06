use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository, WorktreePruneOptions};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let mutate = env::args().any(|argument| argument == "--prune");
    let absolute = std::fs::canonicalize(Path::new(&repository_path))?;
    let virtual_path = absolute
        .strip_prefix("/")
        .expect("absolute path below root");
    let repository = Repository::open(HostFileSystem::new("/")?, virtual_path)?;
    let entries = repository.prune_worktrees(&WorktreePruneOptions {
        dry_run: !mutate,
        ..WorktreePruneOptions::default()
    })?;
    for entry in entries {
        println!(
            "{} {} ({:?})",
            if mutate { "pruned" } else { "would prune" },
            entry.name(),
            entry.reason()
        );
    }
    Ok(())
}
