//! Transactional `update-index` plumbing.

use crate::{Error, IndexEntry, IndexVersion, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateIndexCommand {
    /// Update one literal repository-relative path from the worktree.
    Worktree {
        path: Vec<u8>,
    },
    CacheInfo(IndexEntry),
    Remove {
        path: Vec<u8>,
    },
    Refresh {
        path: Vec<u8>,
        really: bool,
    },
    AssumeUnchanged {
        path: Vec<u8>,
        value: bool,
    },
    SkipWorktree {
        path: Vec<u8>,
        value: bool,
    },
    IntentToAdd {
        path: Vec<u8>,
        value: bool,
    },
    Executable {
        path: Vec<u8>,
        value: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateIndexOptions {
    pub version: Option<IndexVersion>,
    pub allow_add: bool,
    pub allow_remove: bool,
    pub allow_replace: bool,
    pub ignore_skip_worktree: bool,
    /// Compute object IDs but do not store worktree objects (`--info-only`).
    pub info_only: bool,
    pub dry_run: bool,
    pub max_commands: usize,
    pub max_object_size: usize,
}

impl Default for UpdateIndexOptions {
    fn default() -> Self {
        Self {
            version: None,
            allow_add: false,
            allow_remove: false,
            allow_replace: false,
            ignore_skip_worktree: false,
            info_only: false,
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
    #[allow(clippy::too_many_lines)]
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
        let mut pending_blobs = Vec::<(ObjectId, Vec<u8>)>::new();
        for command in commands {
            match command {
                UpdateIndexCommand::Worktree { path } => {
                    self.plan_worktree_update(
                        path,
                        options,
                        &mut entries,
                        &mut pending_blobs,
                        &mut report,
                    )?;
                }
                UpdateIndexCommand::CacheInfo(entry) => {
                    handle_path_collisions(&mut entries, entry.path(), options.allow_replace)?;
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
            if !options.info_only {
                pending_blobs.sort_unstable_by_key(|(id, _)| *id);
                pending_blobs.dedup_by_key(|(id, _)| *id);
                for (_, contents) in pending_blobs {
                    self.write_object(ObjectKind::Blob, &contents)?;
                }
            }
            self.write_index(&replacement)?;
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn plan_worktree_update(
        &self,
        path: &[u8],
        options: &UpdateIndexOptions,
        entries: &mut Vec<IndexEntry>,
        pending_blobs: &mut Vec<(ObjectId, Vec<u8>)>,
        report: &mut UpdateIndexReport,
    ) -> Result<()> {
        let root = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("cannot update paths in a bare repository".into())
        })?;
        let relative = crate::worktree::worktree_path(path)?;
        if crate::worktree::has_symlink_leading_path(self.filesystem(), root, &relative)? {
            return Err(Error::BeyondSymbolicLink(relative));
        }
        let full = root.join(relative);
        let exact = entries
            .iter()
            .filter(|entry| entry.path() == path)
            .cloned()
            .collect::<Vec<_>>();
        if exact.iter().any(IndexEntry::skip_worktree) {
            if options.allow_remove && !options.ignore_skip_worktree {
                entries.retain(|entry| entry.path() != path);
                report.removed.push(path.to_vec());
            }
            return Ok(());
        }
        let metadata = match self.filesystem().metadata(&full) {
            Ok(metadata) => metadata,
            Err(Error::NotFound(_)) => {
                if !options.allow_remove || exact.is_empty() {
                    return Err(Error::InvalidRepository(format!(
                        "path `{}` does not exist; removal was not allowed",
                        String::from_utf8_lossy(path)
                    )));
                }
                entries.retain(|entry| entry.path() != path);
                report.removed.push(path.to_vec());
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            if exact.first().is_some_and(|entry| entry.mode() != 0o160_000) {
                if !options.allow_remove {
                    return Err(Error::InvalidRepository(format!(
                        "path `{}` became a directory; removal was not allowed",
                        String::from_utf8_lossy(path)
                    )));
                }
                entries.retain(|entry| entry.path() != path);
                report.removed.push(path.to_vec());
                return Ok(());
            }
            if entries.iter().any(|entry| {
                entry.path().starts_with(path) && entry.path().get(path.len()) == Some(&b'/')
            }) {
                return Err(Error::InvalidRepository(format!(
                    "path `{}` is a tracked directory; update its files individually",
                    String::from_utf8_lossy(path)
                )));
            }
            if exact.is_empty() && !options.allow_add {
                return Err(missing_add(path));
            }
            let id =
                match Repository::open_shared(self.shared_filesystem(), &full).and_then(|nested| {
                    nested.resolve_revision_id("HEAD", &crate::RevisionOptions::default())
                }) {
                    Ok(id) => id,
                    Err(_) if exact.first().is_some_and(|entry| entry.mode() == 0o160_000) => {
                        return Ok(());
                    }
                    Err(_) => {
                        return Err(Error::InvalidRepository(format!(
                            "path `{}` is a directory, not an initialized Git repository",
                            String::from_utf8_lossy(path)
                        )));
                    }
                };
            replace_path(
                entries,
                path,
                IndexEntry::new(
                    path.to_vec(),
                    0o160_000,
                    id,
                    crate::worktree::index_stat(metadata.stat(), 0),
                )?,
                options.allow_replace,
            )?;
            report.updated.push(path.to_vec());
            return Ok(());
        }
        if exact.is_empty() && !options.allow_add {
            return Err(missing_add(path));
        }
        let (contents, mode) = if metadata.is_symlink() {
            (self.filesystem().read_link(&full)?, 0o120_000)
        } else if metadata.is_file() {
            (
                self.filesystem().read(&full)?,
                if metadata.is_executable() {
                    0o100_755
                } else {
                    0o100_644
                },
            )
        } else {
            return Err(Error::InvalidRepository(
                "unsupported worktree file type".into(),
            ));
        };
        if contents.len() > options.max_object_size {
            return Err(Error::ObjectTooLarge {
                declared: contents.len() as u64,
                limit: options.max_object_size,
            });
        }
        let id = ObjectId::compute(ObjectKind::Blob, &contents);
        let stat = crate::worktree::index_stat(metadata.stat(), metadata.len());
        replace_path(
            entries,
            path,
            IndexEntry::new(path.to_vec(), mode, id, stat)?,
            options.allow_replace,
        )?;
        pending_blobs.push((id, contents));
        report.updated.push(path.to_vec());
        Ok(())
    }
}

fn missing_add(path: &[u8]) -> Error {
    Error::InvalidRepository(format!(
        "path `{}` is not in the index; addition was not allowed",
        String::from_utf8_lossy(path)
    ))
}

fn replace_path(
    entries: &mut Vec<IndexEntry>,
    path: &[u8],
    replacement: IndexEntry,
    allow_replace: bool,
) -> Result<()> {
    handle_path_collisions(entries, path, allow_replace)?;
    entries.retain(|entry| entry.path() != path);
    entries.push(replacement);
    Ok(())
}

fn handle_path_collisions(
    entries: &mut Vec<IndexEntry>,
    path: &[u8],
    allow_replace: bool,
) -> Result<()> {
    let collides = |entry: &IndexEntry| {
        (entry.path().starts_with(path) && entry.path().get(path.len()) == Some(&b'/'))
            || (path.starts_with(entry.path()) && path.get(entry.path().len()) == Some(&b'/'))
    };
    if entries.iter().any(&collides) {
        if !allow_replace {
            return Err(Error::InvalidRepository(format!(
                "path `{}` requires replacement authority",
                String::from_utf8_lossy(path)
            )));
        }
        entries.retain(|entry| !collides(entry));
    }
    Ok(())
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
    use crate::{
        FileSystem, HostFileSystem, Index, InitOptions, MemoryFileSystem, ObjectId, PreviousValue,
        ReferenceName, StatData,
    };
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
    fn cacheinfo_accepts_missing_objects_like_native_git() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let missing = ObjectId::from_bytes([0x11; ObjectId::LENGTH]);
        let command = UpdateIndexCommand::CacheInfo(
            IndexEntry::new(
                b"promised".to_vec(),
                0o100_644,
                missing,
                StatData::default(),
            )
            .unwrap(),
        );
        repository
            .update_index(&[command], &UpdateIndexOptions::default())
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries()[0].id(), missing);
        assert!(!repository.contains_object(missing).unwrap());
    }

    #[test]
    fn updates_tracked_adds_new_and_conditionally_removes_missing_paths() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/tracked"), b"one").unwrap();
        repository
            .update_index(
                &[UpdateIndexCommand::Worktree {
                    path: b"tracked".to_vec(),
                }],
                &UpdateIndexOptions {
                    allow_add: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        fs.write(Path::new("repo/tracked"), b"two").unwrap();
        fs.write(Path::new("repo/new"), b"new").unwrap();
        assert!(
            repository
                .update_index(
                    &[UpdateIndexCommand::Worktree {
                        path: b"new".to_vec()
                    }],
                    &UpdateIndexOptions::default(),
                )
                .is_err()
        );
        repository
            .update_index(
                &[
                    UpdateIndexCommand::Worktree {
                        path: b"tracked".to_vec(),
                    },
                    UpdateIndexCommand::Worktree {
                        path: b"new".to_vec(),
                    },
                ],
                &UpdateIndexOptions {
                    allow_add: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries().len(), 2);
        fs.remove_file(Path::new("repo/tracked")).unwrap();
        assert!(
            repository
                .update_index(
                    &[UpdateIndexCommand::Worktree {
                        path: b"tracked".to_vec()
                    }],
                    &UpdateIndexOptions::default(),
                )
                .is_err()
        );
        repository
            .update_index(
                &[UpdateIndexCommand::Worktree {
                    path: b"tracked".to_vec(),
                }],
                &UpdateIndexOptions {
                    allow_remove: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(repository.read_index().unwrap().entries()[0].path(), b"new");
    }

    #[test]
    fn preflights_every_worktree_path_and_supports_info_only_symlinks() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/good"), b"would be stored")
            .unwrap();
        let id = ObjectId::compute(ObjectKind::Blob, b"would be stored");
        assert!(
            repository
                .update_index(
                    &[
                        UpdateIndexCommand::Worktree {
                            path: b"good".to_vec()
                        },
                        UpdateIndexCommand::Worktree {
                            path: b"missing".to_vec()
                        },
                    ],
                    &UpdateIndexOptions {
                        allow_add: true,
                        ..UpdateIndexOptions::default()
                    },
                )
                .is_err()
        );
        assert!(!repository.contains_object(id).unwrap());
        assert!(repository.read_index().unwrap().entries().is_empty());

        fs.create_symlink(Path::new("repo/link"), b"target")
            .unwrap();
        let link_id = ObjectId::compute(ObjectKind::Blob, b"target");
        repository
            .update_index(
                &[UpdateIndexCommand::Worktree {
                    path: b"link".to_vec(),
                }],
                &UpdateIndexOptions {
                    allow_add: true,
                    info_only: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        let index = repository.read_index().unwrap();
        let entry = &index.entries()[0];
        assert_eq!(entry.mode(), 0o120_000);
        assert_eq!(entry.id(), link_id);
        assert!(!repository.contains_object(link_id).unwrap());
    }

    #[test]
    fn records_initialized_directories_as_gitlinks() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let nested = Repository::init(fs, "repo/sub", &InitOptions::default()).unwrap();
        let tree = nested.write_object(ObjectKind::Tree, b"").unwrap();
        let tip = nested
            .write_object(
                ObjectKind::Commit,
                format!(
                    "tree {tree}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nsubmodule tip\n"
                )
                .as_bytes(),
            )
            .unwrap();
        nested
            .update_reference(
                &ReferenceName::new("refs/heads/main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        repository
            .update_index(
                &[UpdateIndexCommand::Worktree {
                    path: b"sub".to_vec(),
                }],
                &UpdateIndexOptions {
                    allow_add: true,
                    ..UpdateIndexOptions::default()
                },
            )
            .unwrap();
        let index = repository.read_index().unwrap();
        assert_eq!(index.entries()[0].mode(), 0o160_000);
        assert_eq!(index.entries()[0].id(), tip);
    }

    #[test]
    fn refuses_worktree_update_through_a_symlinked_directory() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"top-secret\n").unwrap();
        let fs = HostFileSystem::new(base.path()).unwrap();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.create_symlink(Path::new("repo/link"), outside.to_str().unwrap().as_bytes())
            .unwrap();
        assert!(matches!(
            repository.update_index(
                &[UpdateIndexCommand::Worktree {
                    path: b"link/secret.txt".to_vec(),
                }],
                &UpdateIndexOptions {
                    allow_add: true,
                    ..UpdateIndexOptions::default()
                }
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(repository.read_index().unwrap().entries().is_empty());
        assert!(outside.join("secret.txt").exists());
    }
}
