use std::env;

use git_rs::{HostFileSystem, Result};
use git_rs::{LsTreeOptions, ObjectKind, Repository};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: ls_tree <repository> <tree-ish> [-r] [-t] [-l] [path...]".into(),
        )
    })?;
    let treeish = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: ls_tree <repository> <tree-ish> [-r] [-t] [-l] [path...]".into(),
        )
    })?;
    let mut options = LsTreeOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "-r" => options.recursive = true,
            "-t" => options.show_trees = true,
            "-d" => options.trees_only = true,
            "-l" => options.include_object_size = true,
            _ => options.paths.push(argument.into_bytes()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for entry in repository.ls_tree(&treeish, &options)? {
        let kind = match entry.kind() {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        };
        if options.include_object_size {
            let size = entry
                .object_size()
                .map_or_else(|| "-".into(), |size| size.to_string());
            println!(
                "{:06o} {kind} {} {:>7}\t{}",
                entry.mode_number(),
                entry.id(),
                size,
                String::from_utf8_lossy(entry.path())
            );
        } else {
            println!(
                "{:06o} {kind} {}\t{}",
                entry.mode_number(),
                entry.id(),
                String::from_utf8_lossy(entry.path())
            );
        }
    }
    Ok(())
}
