use std::env;

use git_rs::{
    CloneOptions, HostFileSystem, Repository, RepositoryV2Transport, UploadPackOptions,
    UploadPackV2Limits,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let source_path = arguments
        .next()
        .ok_or("usage: clone_local_v2 <source> <destination> [depth]")?;
    let destination_path = arguments
        .next()
        .ok_or("usage: clone_local_v2 <source> <destination> [depth]")?;
    let depth = arguments.next().map(|value| value.parse()).transpose()?;
    if arguments.next().is_some() {
        return Err("usage: clone_local_v2 <source> <destination> [depth]".into());
    }

    let filesystem = HostFileSystem::new(".")?;
    let source = Repository::open(filesystem.clone(), &source_path)?;
    let mut transport = RepositoryV2Transport::new(
        &source,
        UploadPackOptions::default(),
        UploadPackV2Limits::default(),
    );
    let (_, result) = Repository::clone_from_v2(
        filesystem,
        &destination_path,
        &mut transport,
        &CloneOptions {
            remote_url: source_path,
            depth,
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
