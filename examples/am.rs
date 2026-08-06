use std::{env, path::Path};

use git_rs::{AmOptions, HostFileSystem, Repository, Signature};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let mail_paths = env::args().skip(2).collect::<Vec<_>>();
    assert!(!mail_paths.is_empty(), "usage: am REPOSITORY MAIL...");
    let mails = mail_paths
        .iter()
        .map(std::fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let committer = Signature::new("Example Receiver", "receiver@example.com", 0, 0)?;
    let progress = repository.am(&mails, &committer, &AmOptions::default())?;
    for commit in progress.commits {
        println!("{commit}");
    }
    Ok(())
}
