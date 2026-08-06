use std::env;

use git_rs::{HostFileSystem, ObjectKind, Repository, Signature, TagBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: tag <repository> <name> <target-ref> [message]")?;
    let name = arguments
        .next()
        .ok_or("usage: tag <repository> <name> <target-ref> [message]")?;
    let target = arguments
        .next()
        .ok_or("usage: tag <repository> <name> <target-ref> [message]")?;
    let message = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: tag <repository> <name> <target-ref> [message]".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let target = repository.resolve_reference(&target)?;
    if let Some(message) = message {
        let tag = TagBuilder::new(
            target,
            ObjectKind::Commit,
            &name,
            Signature::new("git-rs", "git-rs@example.com", 0, 0)?,
        )?
        .message(format!("{message}\n").into_bytes())
        .build();
        let (_, id) = repository.create_annotated_tag(&name, &tag, false, 1024 * 1024 * 1024)?;
        println!("{id}");
    } else {
        repository.create_lightweight_tag(&name, target, false, 1024 * 1024 * 1024)?;
        println!("{target}");
    }
    Ok(())
}
