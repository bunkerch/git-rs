use std::env;

use git_rs::{HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: config <repository> <key> [value]")?;
    let key = arguments
        .next()
        .ok_or("usage: config <repository> <key> [value]")?;
    let value = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: config <repository> <key> [value]".into());
    }

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let mut config = repository.read_config()?;
    if let Some(value) = value {
        config.set(&key, value.as_bytes())?;
        repository.write_config(&config)?;
    } else {
        let entry = config.get(&key)?.ok_or("config key not found")?;
        if let Some(value) = entry.value() {
            println!("{}", String::from_utf8_lossy(value));
        } else {
            println!("true");
        }
    }
    Ok(())
}
