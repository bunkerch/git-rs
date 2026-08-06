//! Fast removal of loose objects duplicated by validated packs.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;

use crate::{Error, ObjectId, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrunePackedOptions {
    pub dry_run: bool,
    pub max_pack_files: usize,
    pub max_packed_objects: usize,
    pub max_loose_entries: usize,
}

impl Default for PrunePackedOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            max_pack_files: 100_000,
            max_packed_objects: 10_000_000,
            max_loose_entries: 10_000_000,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrunePackedReport {
    pub removed: Vec<(ObjectId, PathBuf)>,
    pub removed_bytes: u64,
    pub scanned_loose_entries: usize,
    pub packed_objects: usize,
}

impl Repository {
    /// Remove local loose object files whose IDs occur in a validated pack.
    ///
    /// Every pack/index pair is checksum-validated before candidate discovery.
    /// Dry runs execute the same bounded scan without mutating storage.
    ///
    /// # Errors
    /// Returns an error for malformed packs, scan-limit exhaustion, malformed
    /// object fanouts, integer overflow, or storage failures.
    pub fn prune_packed(&self, options: &PrunePackedOptions) -> Result<PrunePackedReport> {
        let packed = self.prune_packed_ids(options)?;
        let objects = self.git_path("objects");
        let mut report = PrunePackedReport {
            packed_objects: packed.len(),
            ..PrunePackedReport::default()
        };
        let mut empty_candidates = Vec::new();
        for fanout in self.filesystem().read_dir(&objects)? {
            let Some(name) = fanout.to_str() else {
                continue;
            };
            if name.len() != 2 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let directory = objects.join(&fanout);
            if !self.filesystem().metadata(&directory)?.is_dir() {
                continue;
            }
            for suffix in self.filesystem().read_dir(&directory)? {
                report.scanned_loose_entries = report
                    .scanned_loose_entries
                    .checked_add(1)
                    .ok_or_else(scan_overflow)?;
                if report.scanned_loose_entries > options.max_loose_entries {
                    return Err(Error::InvalidRepository(
                        "prune-packed loose entry limit exceeded".into(),
                    ));
                }
                let Some(suffix_text) = suffix.to_str() else {
                    continue;
                };
                if suffix_text.len() != 38
                    || !suffix_text.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    continue;
                }
                let path = directory.join(&suffix);
                let metadata = self.filesystem().metadata(&path)?;
                if !metadata.is_file() {
                    continue;
                }
                let id = ObjectId::from_str(&format!("{name}{suffix_text}"))?;
                if packed.contains(&id) {
                    report.removed_bytes = report
                        .removed_bytes
                        .checked_add(metadata.len())
                        .ok_or_else(scan_overflow)?;
                    report.removed.push((id, path));
                }
            }
            empty_candidates.push(directory);
        }
        report.removed.sort_unstable_by_key(|(id, _)| *id);
        if !options.dry_run {
            for (_, path) in &report.removed {
                self.filesystem().remove_file(path)?;
            }
            for directory in empty_candidates {
                match self.filesystem().remove_dir(&directory) {
                    Ok(()) | Err(Error::DirectoryNotEmpty(_) | Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(report)
    }

    fn prune_packed_ids(&self, options: &PrunePackedOptions) -> Result<BTreeSet<ObjectId>> {
        let directory = self.git_path("objects/pack");
        let files = match self.filesystem().read_dir(&directory) {
            Ok(files) => files,
            Err(Error::NotFound(_)) => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        let indexes = files
            .into_iter()
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("idx"))
            .collect::<Vec<_>>();
        if indexes.len() > options.max_pack_files {
            return Err(Error::InvalidRepository(
                "prune-packed pack file limit exceeded".into(),
            ));
        }
        let mut packed = BTreeSet::new();
        for index in indexes {
            let ids = self.validate_pack_pair(&directory.join(index))?;
            if packed.len().saturating_add(ids.len()) > options.max_packed_objects {
                return Err(Error::InvalidRepository(
                    "prune-packed packed object limit exceeded".into(),
                ));
            }
            packed.extend(ids);
        }
        Ok(packed)
    }
}

fn scan_overflow() -> Error {
    Error::InvalidRepository("prune-packed count or size overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem, ObjectKind, PackOptions};

    fn fixture() -> (Repository, MemoryFileSystem, Vec<ObjectId>, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let packed = vec![
            repository.write_object(ObjectKind::Blob, b"one").unwrap(),
            repository.write_object(ObjectKind::Blob, b"two").unwrap(),
        ];
        let loose = repository.write_object(ObjectKind::Blob, b"loose").unwrap();
        repository
            .write_pack(&packed, &PackOptions::default())
            .unwrap();
        (repository, filesystem, packed, loose)
    }

    #[test]
    fn dry_run_reports_only_loose_duplicates() {
        let (repository, _, packed, loose) = fixture();
        let report = repository
            .prune_packed(&PrunePackedOptions::default())
            .unwrap();
        assert_eq!(
            report.removed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            {
                let mut expected = packed.clone();
                expected.sort_unstable();
                expected
            }
        );
        assert_eq!(report.packed_objects, 2);
        assert!(report.removed_bytes > 0);
        assert!(repository.contains_loose_object(packed[0]).unwrap());
        assert!(repository.contains_loose_object(loose).unwrap());
    }

    #[test]
    fn removes_duplicates_and_leaves_objects_pack_readable() {
        let (repository, filesystem, packed, loose) = fixture();
        let report = repository
            .prune_packed(&PrunePackedOptions {
                dry_run: false,
                ..PrunePackedOptions::default()
            })
            .unwrap();
        assert_eq!(report.removed.len(), packed.len());
        for id in packed {
            assert!(!repository.contains_loose_object(id).unwrap());
            assert!(repository.read_object(id, 1024).is_ok());
        }
        assert!(repository.contains_loose_object(loose).unwrap());
        for (_, path) in &report.removed {
            assert!(!filesystem.exists(path.parent().unwrap()).unwrap());
        }
    }

    #[test]
    fn validates_every_pack_before_deleting_and_enforces_limits() {
        let (repository, filesystem, packed, _) = fixture();
        let pack = filesystem
            .read_dir(&repository.git_path("objects/pack"))
            .unwrap()
            .into_iter()
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("pack"))
            .unwrap();
        let path = repository.git_path("objects/pack").join(pack);
        let mut corrupt = filesystem.read(&path).unwrap();
        corrupt[12] ^= 1;
        filesystem.write(&path, &corrupt).unwrap();
        assert!(
            repository
                .prune_packed(&PrunePackedOptions {
                    dry_run: false,
                    ..PrunePackedOptions::default()
                })
                .is_err()
        );
        assert!(
            packed
                .iter()
                .all(|id| repository.contains_loose_object(*id).unwrap())
        );

        let (repository, _, _, _) = fixture();
        assert!(
            repository
                .prune_packed(&PrunePackedOptions {
                    max_packed_objects: 1,
                    ..PrunePackedOptions::default()
                })
                .is_err()
        );
        assert!(
            repository
                .prune_packed(&PrunePackedOptions {
                    max_loose_entries: 0,
                    ..PrunePackedOptions::default()
                })
                .is_err()
        );
    }
}
