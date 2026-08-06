use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{HostFileSystem, NotesOptions, Repository, Signature};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: notes <repository> <target-ref> <message>")?;
    let target_ref = arguments
        .next()
        .ok_or("usage: notes <repository> <target-ref> <message>")?;
    let message = arguments
        .next()
        .ok_or("usage: notes <repository> <target-ref> <message>")?;
    if arguments.next().is_some() {
        return Err("usage: notes <repository> <target-ref> <message>".into());
    }

    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let target = repository.resolve_reference(&target_ref)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let timestamp = i64::try_from(timestamp)?;
    let identity = Signature::new("git-rs", "git-rs@example.com", timestamp, 0)?;
    let note = repository.add_note(
        target,
        message.as_bytes(),
        false,
        &identity,
        &identity,
        &NotesOptions::default(),
    )?;
    println!("{} {}", note.target(), note.blob());
    Ok(())
}
