use std::env;

use git_rs::{HostFileSystem, Repository, RerereOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: rerere REPOSITORY [--autoupdate|status|remaining|clear|forget PATH...]")?;
    let command = arguments.next();
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    match command.as_deref() {
        Some("status") => {
            for path in repository.rerere_status(1_000_000, 1_000_000)? {
                println!("{}", String::from_utf8_lossy(&path));
            }
        }
        Some("remaining") => {
            for path in repository.rerere_remaining(1_000_000, 1_000_000)? {
                println!("{}", String::from_utf8_lossy(&path));
            }
        }
        Some("clear") => repository.rerere_clear(1_000_000, 1_000_000)?,
        Some("forget") => {
            let paths = arguments.map(String::into_bytes).collect::<Vec<_>>();
            repository.rerere_forget(&paths, &RerereOptions::default())?;
        }
        Some("--autoupdate") => {
            repository.rerere(&RerereOptions {
                autoupdate: true,
                ..RerereOptions::default()
            })?;
        }
        Some(argument) => return Err(format!("unknown rerere argument: {argument}").into()),
        None => {
            repository.rerere(&RerereOptions::default())?;
        }
    }
    Ok(())
}
