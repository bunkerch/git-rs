use std::env;

use git_rs::{
    HostFileSystem, IndexEntry, IndexVersion, ObjectKind, Repository, StatData, UpdateIndexCommand,
    UpdateIndexOptions,
};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let path = env::args()
        .nth(2)
        .expect("usage: update_index <repository> <path> <contents>");
    let contents = env::args().nth(3).unwrap_or_default().into_bytes();
    let requested_version = env::args().nth(4);
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    let id = repository.write_object(ObjectKind::Blob, &contents)?;
    let path = path.into_bytes();
    let entry = IndexEntry::new(
        path.clone(),
        0o100_644,
        id,
        StatData {
            size: u32::try_from(contents.len()).unwrap_or(u32::MAX),
            ..StatData::default()
        },
    )?;
    let version = match requested_version.as_deref() {
        None => None,
        Some("2") => Some(IndexVersion::V2),
        Some("3") => Some(IndexVersion::V3),
        Some("4") => Some(IndexVersion::V4),
        Some(value) => {
            return Err(git_rs::Error::InvalidRepository(format!(
                "unsupported requested index version {value}"
            )));
        }
    };
    repository.update_index(
        &[UpdateIndexCommand::CacheInfo(entry)],
        &UpdateIndexOptions {
            version,
            ..UpdateIndexOptions::default()
        },
    )?;
    println!("{id}");
    Ok(())
}
