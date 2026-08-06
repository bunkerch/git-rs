use std::env;

use git_rs::{CloneOptions, HostFileSystem, Repository, RepositoryTransport, UploadPackOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let source_path = arguments
        .next()
        .ok_or("usage: clone_local <source> <destination>")?;
    let destination_path = arguments
        .next()
        .ok_or("usage: clone_local <source> <destination>")?;
    if arguments.next().is_some() {
        return Err("usage: clone_local <source> <destination>".into());
    }

    let filesystem = HostFileSystem::new(".")?;
    let source = Repository::open(filesystem.clone(), &source_path)?;
    let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
    let (_, result) = Repository::clone_from(
        filesystem,
        &destination_path,
        &mut transport,
        &CloneOptions {
            remote_url: source_path,
            ..CloneOptions::default()
        },
    )?;
    println!(
        "received {} objects and updated {} refs",
        result.received_objects,
        result.updated_refs.len()
    );
    Ok(())
}
