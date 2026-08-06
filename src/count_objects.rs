//! Bounded inventory of loose and packed object storage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{Error, ObjectId, PackIndex, Repository, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CountObjectsOptions {
    pub max_loose_entries: usize,
    pub max_pack_files: usize,
}

impl Default for CountObjectsOptions {
    fn default() -> Self {
        Self {
            max_loose_entries: 10_000_000,
            max_pack_files: 100_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectGarbageReason {
    InvalidLooseName,
    NonFileLooseObject,
    UnrecognizedPackFile,
    IncompletePack,
    InvalidPackIndex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectGarbage {
    path: PathBuf,
    size: u64,
    reason: ObjectGarbageReason,
}

impl ObjectGarbage {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn reason(&self) -> ObjectGarbageReason {
        self.reason
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CountObjectsReport {
    pub loose_objects: usize,
    pub loose_bytes: u64,
    pub packed_objects: usize,
    pub packs: usize,
    pub packed_bytes: u64,
    pub prune_packable: usize,
    pub garbage_bytes: u64,
    pub garbage: Vec<ObjectGarbage>,
}

impl Repository {
    /// Count local loose objects, pack pairs, packed entries, loose duplicates,
    /// and malformed or incomplete object-store files in one bounded scan.
    ///
    /// Sizes are logical stored bytes. This is deterministic across filesystem
    /// adapters, unlike host allocation-block sizes reported by Git.
    ///
    /// # Errors
    /// Returns an error for exceeded scan limits, malformed directory storage,
    /// integer overflow, or filesystem failures.
    pub fn count_objects(&self, options: &CountObjectsOptions) -> Result<CountObjectsReport> {
        let objects = self.git_path("objects");
        let mut report = CountObjectsReport::default();
        let mut loose_ids = Vec::new();
        let mut seen_loose = 0_usize;
        for child in self.filesystem().read_dir(&objects)? {
            seen_loose = seen_loose.checked_add(1).ok_or_else(scan_overflow)?;
            if seen_loose > options.max_loose_entries {
                return Err(Error::InvalidRepository(
                    "loose object scan exceeds entry limit".into(),
                ));
            }
            let Some(name) = child.to_str() else {
                push_garbage(
                    self,
                    &objects.join(child),
                    ObjectGarbageReason::InvalidLooseName,
                    &mut report,
                )?;
                continue;
            };
            if matches!(name, "info" | "pack") {
                continue;
            }
            let path = objects.join(&child);
            let metadata = self.filesystem().metadata(&path)?;
            if name.len() == 2
                && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                && metadata.is_dir()
            {
                for suffix in self.filesystem().read_dir(&path)? {
                    seen_loose = seen_loose.checked_add(1).ok_or_else(scan_overflow)?;
                    if seen_loose > options.max_loose_entries {
                        return Err(Error::InvalidRepository(
                            "loose object scan exceeds entry limit".into(),
                        ));
                    }
                    let object_path = path.join(&suffix);
                    let object_metadata = self.filesystem().metadata(&object_path)?;
                    let valid_suffix = suffix.to_str().is_some_and(|value| {
                        value.len() == 38 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    });
                    if valid_suffix && object_metadata.is_file() {
                        let id =
                            ObjectId::from_str(&format!("{name}{}", suffix.to_string_lossy()))?;
                        loose_ids.push(id);
                        report.loose_objects = report
                            .loose_objects
                            .checked_add(1)
                            .ok_or_else(scan_overflow)?;
                        report.loose_bytes = report
                            .loose_bytes
                            .checked_add(object_metadata.len())
                            .ok_or_else(scan_overflow)?;
                    } else {
                        let reason = if valid_suffix {
                            ObjectGarbageReason::NonFileLooseObject
                        } else {
                            ObjectGarbageReason::InvalidLooseName
                        };
                        push_garbage(self, &object_path, reason, &mut report)?;
                    }
                }
            } else {
                push_garbage(
                    self,
                    &path,
                    ObjectGarbageReason::InvalidLooseName,
                    &mut report,
                )?;
            }
        }

        let packed_ids = self.count_pack_directory(options, &mut report)?;
        report.prune_packable = loose_ids
            .iter()
            .filter(|id| packed_ids.contains(id))
            .count();
        Ok(report)
    }

    fn count_pack_directory(
        &self,
        options: &CountObjectsOptions,
        report: &mut CountObjectsReport,
    ) -> Result<BTreeSet<ObjectId>> {
        let directory = self.git_path("objects/pack");
        let files = match self.filesystem().read_dir(&directory) {
            Ok(files) => files,
            Err(Error::NotFound(_)) => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        if files.len() > options.max_pack_files {
            return Err(Error::InvalidRepository(
                "pack directory exceeds file limit".into(),
            ));
        }
        let mut groups: BTreeMap<Vec<u8>, Vec<PathBuf>> = BTreeMap::new();
        for file in files {
            let bytes = file.as_os_str().as_encoded_bytes();
            if bytes == b"multi-pack-index" || bytes == b"multi-pack-index.d" {
                continue;
            }
            if bytes.starts_with(b"multi-pack-index")
                && (bytes.ends_with(b".bitmap") || bytes.ends_with(b".rev"))
            {
                continue;
            }
            let Some(dot) = bytes.iter().rposition(|byte| *byte == b'.') else {
                push_garbage(
                    self,
                    &directory.join(file),
                    ObjectGarbageReason::UnrecognizedPackFile,
                    report,
                )?;
                continue;
            };
            groups.entry(bytes[..dot].to_vec()).or_default().push(file);
        }

        let mut packed_ids = BTreeSet::new();
        for (stem, files) in groups {
            let valid_stem = stem.len() == 45
                && stem.starts_with(b"pack-")
                && stem[5..].iter().all(u8::is_ascii_hexdigit);
            let pack = find_extension(&files, "pack");
            let index = find_extension(&files, "idx");
            let (Some(pack), Some(index)) = (pack, index) else {
                for file in files {
                    push_garbage(
                        self,
                        &directory.join(file),
                        if valid_stem {
                            ObjectGarbageReason::IncompletePack
                        } else {
                            ObjectGarbageReason::UnrecognizedPackFile
                        },
                        report,
                    )?;
                }
                continue;
            };
            let pack = directory.join(pack);
            let index = directory.join(index);
            let Ok(parsed) = PackIndex::parse(&self.filesystem().read(&index)?) else {
                push_garbage(self, &index, ObjectGarbageReason::InvalidPackIndex, report)?;
                push_garbage(self, &pack, ObjectGarbageReason::InvalidPackIndex, report)?;
                continue;
            };
            report.packs = report.packs.checked_add(1).ok_or_else(scan_overflow)?;
            report.packed_objects = report
                .packed_objects
                .checked_add(parsed.entries().len())
                .ok_or_else(scan_overflow)?;
            let pack_bytes = self.filesystem().metadata(&pack)?.len();
            let index_bytes = self.filesystem().metadata(&index)?.len();
            report.packed_bytes = report
                .packed_bytes
                .checked_add(pack_bytes)
                .and_then(|size| size.checked_add(index_bytes))
                .ok_or_else(scan_overflow)?;
            packed_ids.extend(parsed.entries().iter().map(|entry| entry.id));
        }
        Ok(packed_ids)
    }
}

fn find_extension<'a>(files: &'a [PathBuf], extension: &str) -> Option<&'a Path> {
    files
        .iter()
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .map(PathBuf::as_path)
}

fn push_garbage(
    repository: &Repository,
    path: &Path,
    reason: ObjectGarbageReason,
    report: &mut CountObjectsReport,
) -> Result<()> {
    let size = repository.filesystem().metadata(path)?.len();
    report.garbage_bytes = report
        .garbage_bytes
        .checked_add(size)
        .ok_or_else(scan_overflow)?;
    report.garbage.push(ObjectGarbage {
        path: path.to_path_buf(),
        size,
        reason,
    });
    Ok(())
}

fn scan_overflow() -> Error {
    Error::InvalidRepository("object count or size overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem, ObjectKind, PackOptions};

    #[test]
    fn counts_loose_packed_duplicates_and_garbage() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let first = repository.write_object(ObjectKind::Blob, b"first").unwrap();
        let second = repository
            .write_object(ObjectKind::Blob, b"second")
            .unwrap();
        repository
            .write_pack(&[first], &PackOptions::default())
            .unwrap();
        let hex = first.to_string();
        let canonical = Path::new("repo/.git/objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        let uppercase = Path::new("repo/.git/objects")
            .join(hex[..2].to_ascii_uppercase())
            .join(hex[2..].to_ascii_uppercase());
        fs.create_dir_all(uppercase.parent().unwrap()).unwrap();
        fs.write(&uppercase, &fs.read(&canonical).unwrap()).unwrap();
        fs.write(Path::new("repo/.git/objects/pack/temporary"), b"garbage")
            .unwrap();
        fs.create_dir_all(Path::new("repo/.git/objects/aa"))
            .unwrap();
        fs.write(Path::new("repo/.git/objects/aa/not-an-object"), b"bad")
            .unwrap();

        let report = repository
            .count_objects(&CountObjectsOptions::default())
            .unwrap();
        assert_eq!(report.loose_objects, 3);
        assert!(report.loose_bytes > 0);
        assert_eq!(report.packs, 1);
        assert_eq!(report.packed_objects, 1);
        assert_eq!(report.prune_packable, 2);
        assert_eq!(report.garbage.len(), 2);
        assert_eq!(report.garbage_bytes, 10);
        assert!(repository.read_object(second, 1024).is_ok());
    }

    #[test]
    fn reports_incomplete_and_invalid_packs_and_enforces_limits() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(
            Path::new("repo/.git/objects/pack/pack-1111111111111111111111111111111111111111.pack"),
            b"PACK",
        )
        .unwrap();
        let report = repository
            .count_objects(&CountObjectsOptions::default())
            .unwrap();
        assert_eq!(
            report.garbage[0].reason(),
            ObjectGarbageReason::IncompletePack
        );
        assert!(
            repository
                .count_objects(&CountObjectsOptions {
                    max_pack_files: 0,
                    ..CountObjectsOptions::default()
                })
                .is_err()
        );
    }
}
