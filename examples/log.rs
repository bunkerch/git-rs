use std::env;
use std::path::Path;

use git_rs::{HostFileSystem, LogOptions, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let revision = arguments.next().unwrap_or_else(|| "HEAD".to_owned());
    let remaining = arguments.collect::<Vec<_>>();
    let show_patch = remaining.iter().any(|argument| argument == "--patch");
    let diff_merges = remaining.iter().any(|argument| argument == "--diff-merges");
    let path = remaining
        .iter()
        .find(|argument| !argument.starts_with("--"));
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let storage_root = repository_path
        .parent()
        .expect("repository path has a parent");
    let repository_name = repository_path
        .file_name()
        .expect("repository path has a name");
    let repository = Repository::open(HostFileSystem::new(storage_root)?, repository_name)?;
    let tip = repository
        .resolve_revision(
            &format!("{revision}^{{commit}}"),
            &RevisionOptions::default(),
        )?
        .id;
    let options = LogOptions {
        paths: path
            .into_iter()
            .map(|path| path.as_bytes().to_vec())
            .collect(),
        show_patch,
        diff_merges,
        ..Default::default()
    };
    for entry in repository.log(&[tip], &[], &options)? {
        let subject = entry
            .commit()
            .message()
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        println!("{} {}", entry.id(), String::from_utf8_lossy(subject));
        for parent in entry.parent_diffs() {
            print!("{}", String::from_utf8_lossy(parent.patch()));
        }
    }
    Ok(())
}
