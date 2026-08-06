use std::env;

use git_rs::{
    BundleCreateOptions, BundleParseOptions, GitBundle, HostFileSystem, IncomingPackOptions,
    Repository,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("create") => {
            let repository_path = arguments
                .next()
                .ok_or("usage: bundle create <repository> <output> <ref>...")?;
            let output = arguments
                .next()
                .ok_or("usage: bundle create <repository> <output> <ref>...")?;
            let references = arguments.collect::<Vec<_>>();
            let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
            let bundle = repository.create_bundle(&BundleCreateOptions {
                references,
                ..BundleCreateOptions::default()
            })?;
            std::fs::write(output, bundle.encode())?;
        }
        Some("list") => {
            let input = arguments.next().ok_or("usage: bundle list <input>")?;
            if arguments.next().is_some() {
                return Err("usage: bundle list <input>".into());
            }
            let bundle = GitBundle::parse(&std::fs::read(input)?, &BundleParseOptions::default())?;
            for reference in bundle.references() {
                println!("{} {}", reference.id(), reference.name());
            }
        }
        Some("unbundle") => {
            let repository_path = arguments
                .next()
                .ok_or("usage: bundle unbundle <repository> <input>")?;
            let input = arguments
                .next()
                .ok_or("usage: bundle unbundle <repository> <input>")?;
            if arguments.next().is_some() {
                return Err("usage: bundle unbundle <repository> <input>".into());
            }
            let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
            let bundle = GitBundle::parse(&std::fs::read(input)?, &BundleParseOptions::default())?;
            let written =
                repository.unbundle(&bundle, &IncomingPackOptions::default(), 10_000_000)?;
            println!("{}", written.pack_path.display());
        }
        _ => return Err("usage: bundle <create|list|unbundle> ...".into()),
    }
    Ok(())
}
