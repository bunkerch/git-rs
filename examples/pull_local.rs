use std::{env, path::Path};

use git_rs::{
    HostFileSystem, PullMode, PullOptions, Repository, RepositoryTransport, Signature,
    UploadPackOptions,
};

fn main() -> git_rs::Result<()> {
    let source_path = env::args()
        .nth(1)
        .expect("usage: pull_local SOURCE DESTINATION [merge|rebase|ff-only]");
    let destination_path = env::args()
        .nth(2)
        .expect("usage: pull_local SOURCE DESTINATION [merge|rebase|ff-only]");
    let mode = match env::args().nth(3).as_deref() {
        None | Some("ff-only") => PullMode::FastForwardOnly,
        Some("merge") => PullMode::Merge,
        Some("rebase") => PullMode::Rebase,
        Some(value) => panic!("unknown pull mode `{value}`"),
    };
    let source_path = std::fs::canonicalize(Path::new(&source_path))?;
    let source = Repository::open(
        HostFileSystem::new(source_path.parent().expect("source parent"))?,
        source_path.file_name().expect("source name"),
    )?;
    let destination_path = std::fs::canonicalize(Path::new(&destination_path))?;
    let destination = Repository::open(
        HostFileSystem::new(destination_path.parent().expect("destination parent"))?,
        destination_path.file_name().expect("destination name"),
    )?;
    let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
    let committer = Signature::new("Pull Example", "pull@example.com", 0, 0)?;
    let result = destination.pull(
        &mut transport,
        &PullOptions {
            mode,
            ..PullOptions::default()
        },
        &committer,
    )?;
    println!(
        "upstream={} integration={:?}",
        result.upstream, result.integration
    );
    Ok(())
}
