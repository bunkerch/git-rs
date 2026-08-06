use std::env;

use git_rs::{BlameOptions, HostFileSystem, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let revision = arguments.next().unwrap_or_else(|| "HEAD".to_owned());
    let path = arguments.next().expect("repository-relative file path");
    let first_parent = arguments.any(|argument| argument == "--first-parent");
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;

    for line in repository.blame(
        &revision,
        path.as_bytes(),
        &BlameOptions {
            first_parent,
            ..Default::default()
        },
    )? {
        print!(
            "{} {} {} {}\t{}",
            line.commit(),
            line.original_line(),
            line.final_line(),
            line.author().name(),
            String::from_utf8_lossy(line.contents()),
        );
        if !line.contents().ends_with(b"\n") {
            println!();
        }
    }
    Ok(())
}
