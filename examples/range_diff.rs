use std::env;

use git_rs::{HostFileSystem, RangeDiffOptions, RangeDiffStatus, Repository, RevisionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: range_diff <repository> <old-base> <old-tip> <new-base> <new-tip>")?;
    let names = (0..4)
        .map(|_| {
            arguments
                .next()
                .ok_or("usage: range_diff <repository> <old-base> <old-tip> <new-base> <new-tip>")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if arguments.next().is_some() {
        return Err(
            "usage: range_diff <repository> <old-base> <old-tip> <new-base> <new-tip>".into(),
        );
    }
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let revisions = RevisionOptions::default();
    let ids = names
        .iter()
        .map(|name| repository.resolve_revision_id(name, &revisions))
        .collect::<Result<Vec<_>, _>>()?;
    for entry in
        repository.range_diff(ids[0], ids[1], ids[2], ids[3], &RangeDiffOptions::default())?
    {
        let marker = match entry.status() {
            RangeDiffStatus::Equal => '=',
            RangeDiffStatus::Changed => '!',
            RangeDiffStatus::Dropped => '<',
            RangeDiffStatus::Added => '>',
        };
        println!(
            "{} {} {} {}",
            entry
                .old_position()
                .map_or_else(|| "-".into(), |value| value.to_string()),
            marker,
            entry
                .new_position()
                .map_or_else(|| "-".into(), |value| value.to_string()),
            String::from_utf8_lossy(entry.subject())
        );
    }
    Ok(())
}
