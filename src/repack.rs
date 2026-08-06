//! Verified repository-wide pack consolidation.

use std::path::{Path, PathBuf};

use crate::{Error, FsckOptions, ObjectId, PackOptions, Repository, Result, WrittenPack};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepackOptions {
    pub pack: PackOptions,
    /// Include valid objects outside refs, reflogs, the index, and extra roots.
    pub include_unreachable: bool,
    /// Remove selected loose copies only after the new pack is verified.
    pub prune_loose: bool,
    /// Remove prior `.idx`/`.pack` pairs after publishing the complete object set.
    pub delete_redundant_packs: bool,
    /// Discover and validate without publishing or deleting files.
    pub dry_run: bool,
    pub include_index: bool,
    pub include_reflogs: bool,
    pub additional_roots: Vec<ObjectId>,
    pub max_objects: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, FileSystem, InitOptions, MemoryFileSystem, Signature};

    fn fixture() -> (Repository, MemoryFileSystem, Vec<ObjectId>, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/file"), b"content")
            .unwrap();
        repository.add("file").unwrap();
        let blob = repository.read_index().unwrap().entries()[0].id();
        let signature = Signature::new("Repack", "repack@example.com", 100, 0).unwrap();
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
    fn packs_reachable_objects_and_prunes_only_their_loose_copies() {
        let (repository, filesystem, reachable, orphan) = fixture();
        let result = repository
            .repack(&RepackOptions {
                prune_loose: true,
                ..RepackOptions::default()
            })
            .unwrap();
        assert_eq!(result.packed_objects, 3);
        assert_eq!(result.pruned_loose_objects, 3);
        assert_eq!(result.removed_packs, 0);
        assert!(result.pack.is_some());
        for id in reachable {
            assert!(!repository.contains_loose_object(id).unwrap());
            assert!(repository.read_object(id, 1024).is_ok());
        }
        assert!(repository.contains_loose_object(orphan).unwrap());
        assert!(
            filesystem
                .exists(result.pack.as_ref().unwrap().pack_path.as_path())
                .unwrap()
        );
    }

    #[test]
    fn complete_repack_can_replace_old_packs_without_losing_unreachable_objects() {
        let (repository, filesystem, reachable, orphan) = fixture();
        let first = repository
            .write_pack(&reachable, &PackOptions::default())
            .unwrap();
        let result = repository
            .repack(&RepackOptions {
                include_unreachable: true,
                prune_loose: true,
                delete_redundant_packs: true,
                ..RepackOptions::default()
            })
            .unwrap();
        assert_eq!(result.packed_objects, 4);
        assert_eq!(result.pruned_loose_objects, 4);
        assert_eq!(result.removed_packs, 1);
        assert!(!filesystem.exists(&first.index_path).unwrap());
        assert!(!filesystem.exists(&first.pack_path).unwrap());
        assert!(repository.read_object(orphan, 1024).is_ok());
        assert_eq!(repository.fsck(&FsckOptions::default()).unwrap().objects, 4);
    }

    #[test]
    fn rejects_lossy_pack_deletion_and_dry_run_is_immutable() {
        let (repository, filesystem, _, _) = fixture();
        assert!(
            repository
                .repack(&RepackOptions {
                    delete_redundant_packs: true,
                    ..RepackOptions::default()
                })
                .is_err()
        );
        let before = filesystem
            .read_dir(Path::new("repo/.git/objects/pack"))
            .unwrap();
        let result = repository
            .repack(&RepackOptions {
                dry_run: true,
                ..RepackOptions::default()
            })
            .unwrap();
        assert_eq!(result.packed_objects, 3);
        assert!(result.pack.is_none());
        assert_eq!(
            filesystem
                .read_dir(Path::new("repo/.git/objects/pack"))
                .unwrap(),
            before
        );
    }

    #[test]
    fn empty_repository_does_not_publish_an_empty_pack() {
        let filesystem = MemoryFileSystem::new();
        let repository = Repository::init(filesystem, "repo", &InitOptions::default()).unwrap();
        let result = repository.repack(&RepackOptions::default()).unwrap();
        assert_eq!(result.packed_objects, 0);
        assert!(result.pack.is_none());
    }
}

impl Default for RepackOptions {
    fn default() -> Self {
        Self {
            pack: PackOptions::default(),
            include_unreachable: false,
            prune_loose: false,
            delete_redundant_packs: false,
            dry_run: false,
            include_index: true,
            include_reflogs: true,
            additional_roots: Vec::new(),
            max_objects: 10_000_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepackResult {
    pub pack: Option<WrittenPack>,
    pub packed_objects: usize,
    pub pruned_loose_objects: usize,
    pub removed_packs: usize,
}

impl Repository {
    /// Consolidate a verified object set into one Git-compatible pack.
    ///
    /// The replacement pack and index are published and reread successfully
    /// before loose objects or old packs are removed. Old-pack deletion is
    /// accepted only when unreachable objects are included, preventing valid
    /// but unreferenced objects from being discarded accidentally.
    ///
    /// # Errors
    /// Returns an error for an unsafe option combination, failed fsck, pack
    /// construction/publication/verification, or storage mutation failure.
    pub fn repack(&self, options: &RepackOptions) -> Result<RepackResult> {
        if options.delete_redundant_packs && !options.include_unreachable {
            return Err(Error::InvalidRepository(
                "deleting old packs requires include_unreachable=true".into(),
            ));
        }
        let report = self.fsck(&FsckOptions {
            max_object_size: options.pack.max_object_size,
            max_objects: options.max_objects,
            include_index: options.include_index,
            include_reflogs: options.include_reflogs,
            additional_roots: options.additional_roots.clone(),
        })?;
        let mut ids = report.reachable_objects().to_vec();
        if options.include_unreachable {
            ids.extend_from_slice(report.unreachable());
            ids.sort_unstable();
            ids.dedup();
        }
        if ids.is_empty() || options.dry_run {
            return Ok(RepackResult {
                pack: None,
                packed_objects: ids.len(),
                pruned_loose_objects: 0,
                removed_packs: 0,
            });
        }

        let old_packs = self.repack_existing_packs()?;
        let written = self.write_pack(&ids, &options.pack)?;
        let verified =
            self.validate_indexed_pack(&written.index_path, options.pack.max_object_size)?;
        if verified.len() != ids.len() {
            return Err(Error::InvalidObject(
                "published repack object count differs from selection".into(),
            ));
        }

        let mut pruned_loose_objects = 0;
        if options.prune_loose {
            for id in &ids {
                let path = self.repack_loose_path(*id);
                match self.filesystem().remove_file(&path) {
                    Ok(()) => {
                        pruned_loose_objects += 1;
                        if let Some(parent) = path.parent() {
                            match self.filesystem().remove_dir(parent) {
                                Ok(()) | Err(Error::DirectoryNotEmpty(_) | Error::NotFound(_)) => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }

        let mut removed_packs = 0;
        if options.delete_redundant_packs {
            for (index, pack) in old_packs {
                if index == written.index_path && pack == written.pack_path {
                    continue;
                }
                match self.filesystem().remove_file(&index) {
                    Ok(()) => {}
                    Err(Error::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                }
                match self.filesystem().remove_file(&pack) {
                    Ok(()) => removed_packs += 1,
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(RepackResult {
            pack: Some(written),
            packed_objects: ids.len(),
            pruned_loose_objects,
            removed_packs,
        })
    }

    fn repack_existing_packs(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let directory = self.git_path("objects/pack");
        let files = match self.filesystem().read_dir(&directory) {
            Ok(files) => files,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut pairs = Vec::new();
        for file in files {
            if file.extension().and_then(|value| value.to_str()) != Some("idx") {
                continue;
            }
            let index = directory.join(&file);
            let pack = index.with_extension("pack");
            if self.filesystem().exists(&pack)? {
                pairs.push((index, pack));
            }
        }
        Ok(pairs)
    }

    fn repack_loose_path(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_hex();
        let fanout = std::str::from_utf8(&hex[..2]).expect("hex is ASCII");
        let suffix = std::str::from_utf8(&hex[2..]).expect("hex is ASCII");
        self.git_path(Path::new("objects").join(fanout).join(suffix))
    }
}
