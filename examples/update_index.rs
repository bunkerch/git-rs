use std::env;

use git_rs::{HostFileSystem, IndexVersion, Repository, UpdateIndexCommand, UpdateIndexOptions};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let path = arguments
        .next()
        .expect("usage: update_index <repository> <path> [--add] [--remove] [--replace] [--info-only] [--index-version=2|3|4]");
    let mut options = UpdateIndexOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--add" => options.allow_add = true,
            "--remove" => options.allow_remove = true,
            "--replace" => options.allow_replace = true,
            "--info-only" => options.info_only = true,
            "--index-version=2" => options.version = Some(IndexVersion::V2),
            "--index-version=3" => options.version = Some(IndexVersion::V3),
            "--index-version=4" => options.version = Some(IndexVersion::V4),
            value => {
                return Err(git_rs::Error::InvalidRepository(format!(
                    "unsupported update-index option `{value}`"
                )));
            }
        }
    }
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    let report = repository.update_index(
        &[UpdateIndexCommand::Worktree {
            path: path.into_bytes(),
        }],
        &options,
    )?;
    println!(
        "updated={} removed={}",
        report.updated.len(),
        report.removed.len()
    );
    Ok(())
}
