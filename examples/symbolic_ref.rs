use std::env;

use git_rs::{HostFileSystem, PreviousReferenceValue, ReferenceName, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: symbolic_ref <repository> <name> [target]")?;
    let name = arguments
        .next()
        .ok_or("usage: symbolic_ref <repository> <name> [target]")?;
    let target = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: symbolic_ref <repository> <name> [target]".into());
    }

    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    if let Some(target) = target {
        repository.update_symbolic_reference(
            &name,
            &ReferenceName::new(target)?,
            PreviousReferenceValue::Any,
            None,
        )?;
    } else {
        println!("{}", repository.symbolic_reference(&name, true)?);
    }
    Ok(())
}
