use std::env;
use std::io::{self, Write};

use git_rs::{DiffOptions, HostFileSystem, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: diff <repository> <old-ref> <new-ref>")?;
    let old = arguments
        .next()
        .ok_or("usage: diff <repository> <old-ref> <new-ref>")?;
    let new = arguments
        .next()
        .ok_or("usage: diff <repository> <old-ref> <new-ref>")?;
    if arguments.next().is_some() {
        return Err("usage: diff <repository> <old-ref> <new-ref>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let options = DiffOptions::default();
    let old = repository.read_commit(
        repository.resolve_revision_id(&old, &RevisionOptions::default())?,
        options.max_object_size,
    )?;
    let new = repository.read_commit(
        repository.resolve_revision_id(&new, &RevisionOptions::default())?,
        options.max_object_size,
    )?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for entry in repository.diff_trees(Some(old.tree()), Some(new.tree()), &options)? {
        output.write_all(&repository.render_patch(&entry, &options)?)?;
    }
    Ok(())
}
