use std::env;

use git_rs::{
    HostFileSystem, InProcessReceivePackTransport, PushOptions, PushUpdate, ReceivePackOptions,
    ReferenceName, Repository,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let source_path = arguments
        .next()
        .ok_or("usage: push_local <source> <remote> <source-ref> <destination-ref>")?;
    let remote_path = arguments
        .next()
        .ok_or("usage: push_local <source> <remote> <source-ref> <destination-ref>")?;
    let source_ref = arguments
        .next()
        .ok_or("usage: push_local <source> <remote> <source-ref> <destination-ref>")?;
    let destination = ReferenceName::new(
        arguments
            .next()
            .ok_or("usage: push_local <source> <remote> <source-ref> <destination-ref>")?,
    )?;
    if arguments.next().is_some() {
        return Err("usage: push_local <source> <remote> <source-ref> <destination-ref>".into());
    }

    let filesystem = HostFileSystem::new(".")?;
    let source = Repository::open(filesystem.clone(), source_path)?;
    let remote = Repository::open(filesystem, remote_path)?;
    let new_id = source.resolve_reference(&source_ref)?;
    let mut transport = InProcessReceivePackTransport::new(&remote, ReceivePackOptions::default());
    let result = source.push(
        &mut transport,
        &[PushUpdate::update(destination, new_id)],
        &PushOptions::default(),
    )?;
    for status in result.statuses {
        match status.error {
            Some(error) => println!("rejected {}: {error}", status.name),
            None => println!("updated {}", status.name),
        }
    }
    println!("sent {} objects", result.sent_objects);
    Ok(())
}
