use std::env;

use git_rs::{HostFileSystem, MergeTreeOptions, ObjectId, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: merge_tree <repository> <ours> <theirs>")?;
    let ours = arguments
        .next()
        .ok_or("usage: merge_tree <repository> <ours> <theirs>")?
        .parse::<ObjectId>()?;
    let theirs = arguments
        .next()
        .ok_or("usage: merge_tree <repository> <ours> <theirs>")?
        .parse::<ObjectId>()?;
    if arguments.next().is_some() {
        return Err("usage: merge_tree <repository> <ours> <theirs>".into());
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let result = repository.merge_tree(ours, theirs, &MergeTreeOptions::default())?;
    println!("{}", result.tree);
    for conflict in result.conflicts {
        for stage in conflict.stages {
            println!(
                "{:06o} {} {} {}",
                stage.mode,
                stage.id,
                stage.stage,
                String::from_utf8_lossy(&stage.path)
            );
        }
    }
    Ok(())
}
