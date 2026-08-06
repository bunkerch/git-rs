use std::{env, path::Path};

use git_rs::{GcOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let object_expiry = |prefix: &str| {
        arguments
            .iter()
            .find_map(|argument| argument.strip_prefix(prefix))
            .map(str::parse::<u64>)
            .transpose()
            .expect("expiry must be Unix seconds")
    };
    let reflog_expiry = |prefix: &str| {
        arguments
            .iter()
            .find_map(|argument| argument.strip_prefix(prefix))
            .map(str::parse::<i64>)
            .transpose()
            .expect("reflog expiry must be Unix seconds")
    };
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let dry_run = !arguments.iter().any(|argument| argument == "--run");
    let report = repository.gc(&GcOptions {
        dry_run,
        force: !dry_run,
        reflog_expire_before: reflog_expiry("--reflog-before="),
        reflog_expire_unreachable_before: reflog_expiry("--unreachable-reflog-before="),
        worktree_expire_before: object_expiry("--worktree-before="),
        prune_expire_before: object_expiry("--prune-before="),
        ..GcOptions::default()
    })?;
    println!(
        "refs={} reflog-entries={} worktrees={} loose={}",
        report.refs_selected,
        report.reflog_entries_removed,
        report.worktrees.len(),
        report.loose_objects.len(),
    );
    if let Some(repack) = report.repack {
        println!(
            "packed={} pruned-loose={} removed-packs={}",
            repack.packed_objects, repack.pruned_loose_objects, repack.removed_packs,
        );
    }
    Ok(())
}
