use std::env;

use git_rs::{
    DiffKind, HostFileSystem, LayerDiffEntry, LayerDiffOptions, Repository, RevisionOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: diff_layers REPOSITORY --files|--cached|--worktree [TREEISH]")?;
    let mode = arguments
        .next()
        .ok_or("usage: diff_layers REPOSITORY --files|--cached|--worktree [TREEISH]")?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let options = LayerDiffOptions::default();
    let entries = if mode == "--files" {
        repository.diff_files(&options)?
    } else {
        let revision = arguments
            .next()
            .ok_or("cached/worktree mode requires a tree-ish")?;
        let id = repository.resolve_revision_id(&revision, &RevisionOptions::default())?;
        let tree = repository
            .read_commit(id, options.diff.max_object_size)?
            .tree();
        repository.diff_index(Some(tree), mode == "--cached", &options)?
    };
    for entry in entries {
        match entry {
            LayerDiffEntry::Change(change) => {
                let status = match change.kind() {
                    DiffKind::Added => "A",
                    DiffKind::Deleted => "D",
                    DiffKind::Modified => "M",
                    DiffKind::TypeChanged => "T",
                    DiffKind::Renamed => "R100",
                };
                let path = change.new_path().or_else(|| change.old_path()).unwrap();
                println!("{status}\t{}", String::from_utf8_lossy(path));
            }
            LayerDiffEntry::Unmerged { path, .. } => {
                println!("U\t{}", String::from_utf8_lossy(&path));
            }
        }
    }
    Ok(())
}
