use std::env;

use git_rs::{HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: remote <repository> <list|add|rename|remove> [arguments]")?;
    let command = arguments
        .next()
        .ok_or("usage: remote <repository> <list|add|rename|remove> [arguments]")?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    match command.as_str() {
        "list" => {
            if arguments.next().is_some() {
                return Err("usage: remote <repository> list".into());
            }
            for remote in repository.remotes()? {
                println!("{}", remote.name());
            }
        }
        "add" => {
            let name = arguments.next().ok_or("remote add requires NAME URL")?;
            let url = arguments.next().ok_or("remote add requires NAME URL")?;
            if arguments.next().is_some() {
                return Err("remote add requires NAME URL".into());
            }
            repository.add_remote(&name, url.as_bytes())?;
        }
        "rename" => {
            let old = arguments.next().ok_or("remote rename requires OLD NEW")?;
            let new = arguments.next().ok_or("remote rename requires OLD NEW")?;
            if arguments.next().is_some() {
                return Err("remote rename requires OLD NEW".into());
            }
            repository.rename_remote(&old, &new)?;
        }
        "remove" => {
            let name = arguments.next().ok_or("remote remove requires NAME")?;
            if arguments.next().is_some() {
                return Err("remote remove requires NAME".into());
            }
            let result = repository.remove_remote(&name)?;
            println!("removed {} tracking refs", result.removed_refs.len());
        }
        _ => return Err("usage: remote <repository> <list|add|rename|remove> [arguments]".into()),
    }
    Ok(())
}
