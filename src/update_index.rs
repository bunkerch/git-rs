//! Transactional `update-index` plumbing.

use crate::{Error, IndexEntry, IndexVersion, ObjectKind, Repository, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateIndexCommand {
    CacheInfo(IndexEntry),
    Remove { path: Vec<u8> },
    Refresh { path: Vec<u8>, really: bool },
    AssumeUnchanged { path: Vec<u8>, value: bool },
    SkipWorktree { path: Vec<u8>, value: bool },
    IntentToAdd { path: Vec<u8>, value: bool },
    Executable { path: Vec<u8>, value: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateIndexOptions {
    pub version: Option<IndexVersion>,
    pub dry_run: bool,
    pub max_commands: usize,
    pub max_object_size: usize,
}

impl Default for UpdateIndexOptions {
    fn default() -> Self {
        Self {
            version: None,
            dry_run: false,
            max_commands: 1_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateIndexReport {
    pub updated: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
    pub refreshed: Vec<Vec<u8>>,
    pub needs_update: Vec<Vec<u8>>,
}

impl Repository {
    /// Apply typed index commands and atomically publish the resulting index.
    ///
    /// Cache entries are checked against the object database. Refresh verifies
    /// worktree content before changing stat data, as Git does.
    ///
    /// # Errors
    /// Returns an error for excessive commands, absent flag targets, invalid
    /// objects, unsafe paths, bare repositories, or storage failures.
    pub fn update_index(
        &self,
        commands: &[UpdateIndexCommand],
        options: &UpdateIndexOptions,
    ) -> Result<UpdateIndexReport> {
        if commands.len() > options.max_commands {
            return Err(Error::InvalidRepository(
                "update-index command limit exceeded".into(),
            ));
        }
        let current = self.read_index()?;
        let mut entries = current.entries().to_vec();
        let mut report = UpdateIndexReport::default();
        let mut needs_extended = false;
        for command in commands {
            match command {
                UpdateIndexCommand::CacheInfo(entry) => {
                    let object = self.read_object(entry.id(), options.max_object_size)?;
                    let expected = if entry.mode() == 0o160_000 {
                        ObjectKind::Commit
                    } else {
                        ObjectKind::Blob
                    };
                    if object.kind() != expected {
                        return Err(Error::InvalidRepository(format!(
                            "index mode {:o} is incompatible with {} object",
                            entry.mode(),
                            entry.id()
                        )));
                    }
                    entries
                        .retain(|old| old.path() != entry.path() || old.stage() != entry.stage());
                    entries.push(entry.clone());
                    needs_extended |= entry.intent_to_add() || entry.skip_worktree();
                    report.updated.push(entry.path().to_vec());
                }
                UpdateIndexCommand::Remove { path } => {
                    let before = entries.len();
                    entries.retain(|entry| entry.path() != path);
                    if entries.len() != before {
                        report.removed.push(path.clone());
                    }
                }
                UpdateIndexCommand::Refresh { path, really } => {
                    let position = stage_zero(&entries, path)?;
                    if entries[position].assume_valid() && !really {
                        continue;
                    }
                    let root = self.work_tree().ok_or_else(|| {
                        Error::InvalidRepository("cannot refresh a bare repository".into())
                    })?;
                    let full = root.join(crate::worktree::worktree_path(path)?);
                    if !self
                        .worktree_matches(&entries[position], &full)
                        .unwrap_or(false)
                    {
                        report.needs_update.push(path.clone());
                        continue;
                    }
                    let metadata = self.filesystem().metadata(&full)?;
                    entries[position] = entries[position]
                        .clone()
                        .with_stat(crate::worktree::index_stat(metadata.stat(), metadata.len()));
                    report.refreshed.push(path.clone());
                }
                UpdateIndexCommand::AssumeUnchanged { path, value } => {
                    let position = stage_zero(&entries, path)?;
                    entries[position] = entries[position].clone().with_assume_valid(*value);
                    report.updated.push(path.clone());
                }
                UpdateIndexCommand::SkipWorktree { path, value } => {
                    let position = stage_zero(&entries, path)?;
                    entries[position] = entries[position].clone().with_skip_worktree(*value);
                    needs_extended |= *value;
                    report.updated.push(path.clone());
                }
                UpdateIndexCommand::IntentToAdd { path, value } => {
                    let position = stage_zero(&entries, path)?;
                    entries[position] = entries[position].clone().with_intent_to_add(*value);
                    needs_extended |= *value;
                    report.updated.push(path.clone());
                }
                UpdateIndexCommand::Executable { path, value } => {
                    let position = stage_zero(&entries, path)?;
                    entries[position] = entries[position].clone().with_executable(*value)?;
                    report.updated.push(path.clone());
                }
            }
        }
        let version = options.version.unwrap_or_else(|| {
            if needs_extended && current.version() == IndexVersion::V2 {
                IndexVersion::V3
            } else {
                current.version()
            }
        });
        let replacement = current.with_version_and_entries(version, entries)?;
        if !options.dry_run {
            self.write_index(&replacement)?;
        }
        Ok(report)
    }
}

fn stage_zero(entries: &[IndexEntry], path: &[u8]) -> Result<usize> {
    entries
        .iter()
        .position(|entry| entry.path() == path && entry.stage() == 0)
        .ok_or_else(|| {
            Error::InvalidRepository(format!(
                "path `{}` has no stage-zero index entry",
                String::from_utf8_lossy(path)
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, Index, InitOptions, MemoryFileSystem, ObjectId, StatData};
    use std::path::Path;

    #[test]
    fn applies_cache_flags_mode_refresh_and_remove_atomically() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/file"), b"contents").unwrap();
        fs.set_executable(Path::new("repo/file"), true).unwrap();
        let id = repository
            .write_object(ObjectKind::Blob, b"contents")
            .unwrap();
        let entry = IndexEntry::new(b"file".to_vec(), 0o100_644, id, StatData::default()).unwrap();
        repository
            .update_index(
                &[
                    UpdateIndexCommand::CacheInfo(entry),
                    UpdateIndexCommand::AssumeUnchanged {
                        path: b"file".to_vec(),
                        value: true,
                    },
                    UpdateIndexCommand::SkipWorktree {
                        path: b"file".to_vec(),
                        value: true,
                    },
                    UpdateIndexCommand::Executable {
                        path: b"file".to_vec(),
                        value: true,
                    },
                    UpdateIndexCommand::Refresh {
                        path: b"file".to_vec(),
                        really: true,
                    },
                ],
                &UpdateIndexOptions::default(),
            )
            .unwrap();
        let index = repository.read_index().unwrap();
        assert_eq!(index.version(), IndexVersion::V3);
        assert_eq!(index.entries()[0].mode(), 0o100_755);
        assert!(index.entries()[0].assume_valid());
        assert!(index.entries()[0].skip_worktree());
        assert_eq!(index.entries()[0].stat().size, 8);

        repository
            .update_index(
                &[UpdateIndexCommand::Remove {
                    path: b"file".to_vec(),
                }],
                &UpdateIndexOptions::default(),
            )
            .unwrap();
        assert!(repository.read_index().unwrap().entries().is_empty());
    }

    #[test]
    fn reports_dirty_refresh_and_dry_run_does_not_publish() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/file"), b"changed").unwrap();
        let id = repository
            .write_object(ObjectKind::Blob, b"indexed")
            .unwrap();
        let entry = IndexEntry::new(b"file".to_vec(), 0o100_644, id, StatData::default()).unwrap();
        repository
            .write_index(&Index::new(IndexVersion::V2, vec![entry]).unwrap())
            .unwrap();
        let report = repository
            .update_index(
                &[UpdateIndexCommand::Refresh {
                    path: b"file".to_vec(),
                    really: false,
                }],
                &UpdateIndexOptions::default(),
            )
            .unwrap();
        assert_eq!(report.needs_update, vec![b"file".to_vec()]);

        let other = repository.write_object(ObjectKind::Blob, b"other").unwrap();
        repository
            .update_index(
                &[UpdateIndexCommand::CacheInfo(
                    IndexEntry::new(b"other".to_vec(), 0o100_644, other, StatData::default())
                        .unwrap(),
                )],
                &UpdateIndexOptions {
                    dry_run: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries().len(), 1);
    }

    #[test]
    fn rejects_wrong_object_kind_before_publication() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let tree = repository.write_object(ObjectKind::Tree, b"").unwrap();
        let command = UpdateIndexCommand::CacheInfo(
            IndexEntry::new(b"bad".to_vec(), 0o100_644, tree, StatData::default()).unwrap(),
        );
        assert!(
            repository
                .update_index(&[command], &UpdateIndexOptions::default())
                .is_err()
        );
        assert!(repository.read_index().unwrap().entries().is_empty());
        assert_ne!(tree, ObjectId::null());
    }
}
