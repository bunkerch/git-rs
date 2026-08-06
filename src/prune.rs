//! Safe discovery and pruning of loose object files.

use std::path::{Path, PathBuf};

use crate::{Error, FsckOptions, ObjectId, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PruneReason {
    Unreachable,
    PackedDuplicate,
    Temporary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneEntry {
    id: Option<ObjectId>,
    path: PathBuf,
    modified_seconds: u64,
    reason: PruneReason,
    directory: bool,
}

impl PruneEntry {
    #[must_use]
    pub const fn id(&self) -> Option<ObjectId> {
        self.id
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub const fn modified_seconds(&self) -> u64 {
        self.modified_seconds
    }
    #[must_use]
    pub const fn reason(&self) -> PruneReason {
        self.reason
    }
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.directory
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneOptions {
    /// Unreachable loose objects with mtime at or before this value are eligible.
    /// A mutating call must supply this explicitly.
    pub expire_before: Option<u64>,
    /// Remove loose copies of objects verified in at least one pack.
    pub prune_packed_copies: bool,
    pub include_index: bool,
    pub include_reflogs: bool,
    pub additional_roots: Vec<ObjectId>,
    pub max_object_size: usize,
    pub max_objects: usize,
    pub force: bool,
    pub dry_run: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            expire_before: None,
            prune_packed_copies: true,
            include_index: true,
            include_reflogs: true,
            additional_roots: Vec::new(),
            max_object_size: 1024 * 1024 * 1024,
            max_objects: 10_000_000,
            force: false,
            dry_run: true,
        }
    }
}

impl Repository {
    /// Discover and optionally remove eligible loose object files.
    ///
    /// Full fsck completes before selection. Mutation requires `force`, an
    /// explicit expiry threshold, and an exclusive `gc.pid` maintenance lock.
    /// Packed files are never removed by this operation.
    ///
    /// # Errors
    /// Returns an error for unsafe mutation options, failed verification,
    /// maintenance lock contention, storage metadata, or deletion failure.
    pub fn prune(&self, options: &PruneOptions) -> Result<Vec<PruneEntry>> {
        self.validate_prune_options(options)?;
        if options.dry_run {
            return self.prune_discover(options);
        }
        let lock = self.git_path("gc.pid");
        self.filesystem().write_new(&lock, b"git-rs prune\n")?;
        let result = self.prune_under_maintenance_lock(options);
        let cleanup = self.filesystem().remove_file(&lock);
        match (result, cleanup) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(entries), Ok(())) => Ok(entries),
        }
    }

    pub(crate) fn prune_under_maintenance_lock(
        &self,
        options: &PruneOptions,
    ) -> Result<Vec<PruneEntry>> {
        self.validate_prune_options(options)?;
        let entries = self.prune_discover(options)?;
        if options.dry_run {
            return Ok(entries);
        }
        for entry in &entries {
            if entry.directory {
                remove_prune_tree(self, &entry.path)?;
            } else {
                self.filesystem().remove_file(&entry.path)?;
            }
            if let Some(parent) = entry.path.parent() {
                match self.filesystem().remove_dir(parent) {
                    Ok(()) | Err(Error::DirectoryNotEmpty(_) | Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(entries)
    }

    fn validate_prune_options(&self, options: &PruneOptions) -> Result<()> {
        let config = self.read_config()?;
        if config.get("extensions.preciousobjects")?.is_some()
            && config.get_bool("extensions.preciousobjects")?
        {
            return Err(Error::InvalidRepository(
                "cannot prune a precious-objects repository".into(),
            ));
        }
        if !options.dry_run && (!options.force || options.expire_before.is_none()) {
            return Err(Error::InvalidRepository(
                "prune mutation requires force=true and an explicit expiration".into(),
            ));
        }
        Ok(())
    }

    fn prune_discover(&self, options: &PruneOptions) -> Result<Vec<PruneEntry>> {
        let report = self.fsck(&FsckOptions {
            max_object_size: options.max_object_size,
            max_objects: options.max_objects,
            include_index: options.include_index,
            include_reflogs: options.include_reflogs,
            additional_roots: options.additional_roots.clone(),
        })?;
        let packed = report.packed_objects();
        let mut candidates = report
            .unreachable()
            .iter()
            .copied()
            .map(|id| (id, PruneReason::Unreachable))
            .collect::<Vec<_>>();
        if options.prune_packed_copies {
            candidates.extend(
                packed
                    .iter()
                    .copied()
                    .map(|id| (id, PruneReason::PackedDuplicate)),
            );
        }
        candidates.sort_unstable_by_key(|(id, reason)| (*id, reason_rank(*reason)));
        candidates.dedup_by_key(|(id, _)| *id);
        let mut entries = Vec::new();
        for (id, reason) in candidates {
            let path = loose_object_path(self, id);
            let metadata = match self.filesystem().metadata(&path) {
                Ok(metadata) => metadata,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if !metadata.is_file() {
                return Err(Error::InvalidPath(path));
            }
            let modified_seconds = u64::from(metadata.stat().mtime_seconds);
            if reason == PruneReason::Unreachable
                && !unreachable_expired(modified_seconds, options.expire_before)
            {
                continue;
            }
            entries.push(PruneEntry {
                id: Some(id),
                path,
                modified_seconds,
                reason,
                directory: false,
            });
        }
        self.collect_temporary_prune_entries(options, &mut entries)?;
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    fn collect_temporary_prune_entries(
        &self,
        options: &PruneOptions,
        output: &mut Vec<PruneEntry>,
    ) -> Result<()> {
        for directory in [self.git_path("objects"), self.git_path("objects/pack")] {
            let children = match self.filesystem().read_dir(&directory) {
                Ok(children) => children,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            for child in children {
                if !child.to_string_lossy().starts_with("tmp_") {
                    continue;
                }
                let path = directory.join(child);
                let metadata = self.filesystem().metadata(&path)?;
                let modified_seconds = u64::from(metadata.stat().mtime_seconds);
                if !unreachable_expired(modified_seconds, options.expire_before) {
                    continue;
                }
                if output.len() >= options.max_objects {
                    return Err(Error::InvalidRepository(
                        "prune entry limit exceeded".into(),
                    ));
                }
                output.push(PruneEntry {
                    id: None,
                    path,
                    modified_seconds,
                    reason: PruneReason::Temporary,
                    directory: metadata.is_dir(),
                });
            }
        }
        Ok(())
    }
}

const fn reason_rank(reason: PruneReason) -> u8 {
    match reason {
        PruneReason::PackedDuplicate => 0,
        PruneReason::Unreachable => 1,
        PruneReason::Temporary => 2,
    }
}

fn loose_object_path(repository: &Repository, id: ObjectId) -> PathBuf {
    let hex = id.to_hex();
    let fanout = std::str::from_utf8(&hex[..2]).expect("hex is ASCII");
    let suffix = std::str::from_utf8(&hex[2..]).expect("hex is ASCII");
    repository.git_path(Path::new("objects").join(fanout).join(suffix))
}

fn remove_prune_tree(repository: &Repository, path: &Path) -> Result<()> {
    for child in repository.filesystem().read_dir(path)? {
        let child = path.join(child);
        if repository.filesystem().metadata(&child)?.is_dir() {
            remove_prune_tree(repository, &child)?;
        } else {
            repository.filesystem().remove_file(&child)?;
        }
    }
    repository.filesystem().remove_dir(path)
}

const fn unreachable_expired(modified: u64, expiry: Option<u64>) -> bool {
    match expiry {
        Some(expiry) => modified <= expiry,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, FileSystem, InitOptions, MemoryFileSystem, PackOptions, Signature};

    fn fixture() -> (Repository, MemoryFileSystem, Vec<ObjectId>, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/file"), b"content")
            .unwrap();
        repository.add("file").unwrap();
        let blob = repository.read_index().unwrap().entries()[0].id();
        let signature = Signature::new("Prune", "prune@example.com", 100, 0).unwrap();
        let commit = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        let tree = repository.read_commit(commit, 1024).unwrap().tree();
        let orphan = repository
            .write_object(crate::ObjectKind::Blob, b"orphan")
            .unwrap();
        (repository, filesystem, vec![commit, tree, blob], orphan)
    }

    #[test]
    fn dry_run_reports_only_unreachable_loose_objects_by_default() {
        let (repository, _, _, orphan) = fixture();
        let entries = repository.prune(&PruneOptions::default()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id(), Some(orphan));
        assert_eq!(entries[0].reason(), PruneReason::Unreachable);
        assert!(repository.contains_loose_object(orphan).unwrap());
        let retained = repository
            .prune(&PruneOptions {
                additional_roots: vec![orphan],
                ..PruneOptions::default()
            })
            .unwrap();
        assert!(retained.is_empty());
    }

    #[test]
    fn packed_duplicates_are_safe_candidates_and_remain_readable() {
        let (repository, _, mut reachable, orphan) = fixture();
        reachable.push(orphan);
        repository
            .write_pack(&reachable, &PackOptions::default())
            .unwrap();
        let removed = repository
            .prune(&PruneOptions {
                expire_before: Some(0),
                force: true,
                dry_run: false,
                ..PruneOptions::default()
            })
            .unwrap();
        assert_eq!(removed.len(), 4);
        assert!(
            removed
                .iter()
                .all(|entry| entry.reason() == PruneReason::PackedDuplicate)
        );
        for id in reachable {
            assert!(!repository.contains_loose_object(id).unwrap());
            assert!(repository.read_object(id, 1024).is_ok());
        }
        assert!(
            !repository
                .filesystem()
                .exists(&repository.git_path("gc.pid"))
                .unwrap()
        );
    }

    #[test]
    fn mutation_requires_authority_and_respects_maintenance_lock() {
        let (repository, filesystem, _, orphan) = fixture();
        assert!(
            repository
                .prune(&PruneOptions {
                    dry_run: false,
                    ..PruneOptions::default()
                })
                .is_err()
        );
        filesystem
            .write(Path::new("repo/.git/gc.pid"), b"other maintenance\n")
            .unwrap();
        assert!(
            repository
                .prune(&PruneOptions {
                    expire_before: Some(0),
                    force: true,
                    dry_run: false,
                    ..PruneOptions::default()
                })
                .is_err()
        );
        assert!(repository.contains_loose_object(orphan).unwrap());
    }

    #[test]
    fn expiration_boundary_is_inclusive() {
        assert!(unreachable_expired(10, None));
        assert!(unreachable_expired(10, Some(10)));
        assert!(unreachable_expired(10, Some(11)));
        assert!(!unreachable_expired(10, Some(9)));
    }

    #[test]
    fn removes_expired_temporary_files_and_directories() {
        let (repository, filesystem, _, _) = fixture();
        filesystem
            .write(Path::new("repo/.git/objects/tmp_object"), b"temporary")
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/.git/objects/pack/tmp_pack/nested"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/.git/objects/pack/tmp_pack/nested/file"),
                b"temporary",
            )
            .unwrap();
        let removed = repository
            .prune(&PruneOptions {
                expire_before: Some(0),
                force: true,
                dry_run: false,
                ..PruneOptions::default()
            })
            .unwrap();
        assert_eq!(
            removed
                .iter()
                .filter(|entry| entry.reason() == PruneReason::Temporary)
                .count(),
            2
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/objects/tmp_object"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/objects/pack/tmp_pack"))
                .unwrap()
        );
    }

    #[test]
    fn precious_objects_repository_rejects_even_dry_run() {
        let (repository, _, _, _) = fixture();
        let mut config = repository.read_config().unwrap();
        config.set("extensions.preciousObjects", b"true").unwrap();
        repository.write_config(&config).unwrap();
        assert!(repository.prune(&PruneOptions::default()).is_err());
    }
}
