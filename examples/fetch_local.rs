use std::env;

use git_rs::{FetchOptions, HostFileSystem, Repository, RepositoryTransport, UploadPackOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let source = arguments
        .next()
        .ok_or("usage: fetch_local <source> <destination> [depth|+deepen]")?;
    let destination = arguments
        .next()
        .ok_or("usage: fetch_local <source> <destination> [depth|+deepen]")?;
    let value = arguments.next();
    let depth = value
        .as_deref()
        .filter(|value| !value.starts_with('+'))
        .map(str::parse)
        .transpose()?;
    let deepen = value
        .as_deref()
        .and_then(|value| value.strip_prefix('+'))
        .map(str::parse)
        .transpose()?;
    if arguments.next().is_some() {
        return Err("usage: fetch_local <source> <destination> [depth|+deepen]".into());
    }
    let filesystem = HostFileSystem::new(".")?;
    let remote = Repository::open(filesystem.clone(), source)?;
    let local = Repository::open(filesystem, destination)?;
    let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
    let result = local.fetch(
        &mut transport,
        &FetchOptions {
            depth,
            deepen,
            ..FetchOptions::default()
        },
    )?;
    println!(
        "received={} shallow={}",
        result.received_objects,
        result.shallow_commits.len()
    );
    Ok(())
}
