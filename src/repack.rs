//! Verified repository-wide pack consolidation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{
    CruftMtimes, Error, FsckOptions, ObjectId, PackIndex, PackOptions, Repository, Result,
    WrittenPack,
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepackOptions {
    pub pack: PackOptions,
    /// Include valid objects outside refs, reflogs, the index, and extra roots.
    pub include_unreachable: bool,
    /// Store unreachable objects separately with Git-compatible per-object
    /// mtimes, allowing safe old-pack replacement.
    pub cruft: bool,
    /// With `cruft`, omit unreachable objects whose preserved mtime is at or
    /// before this Unix timestamp.
    pub cruft_expire_before: Option<u64>,
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
    fn cruft_repack_separates_preserves_and_explicitly_expires_unreachable_objects() {
        let (repository, filesystem, reachable, orphan) = fixture();
        let old = repository
            .write_pack(
                &reachable
                    .iter()
                    .copied()
                    .chain([orphan])
                    .collect::<Vec<_>>(),
                &PackOptions::default(),
            )
            .unwrap();
        filesystem
            .write(&old.pack_path.with_extension("rev"), b"sidecar")
            .unwrap();
        let result = repository
            .repack(&RepackOptions {
                cruft: true,
                prune_loose: true,
                delete_redundant_packs: true,
                ..RepackOptions::default()
            })
            .unwrap();
        assert_eq!(result.pack.as_ref().unwrap().object_count, 3);
        let cruft = result.cruft_pack.as_ref().unwrap();
        assert_eq!(cruft.object_count, 1);
        let index = PackIndex::parse(&filesystem.read(&cruft.index_path).unwrap()).unwrap();
        let mtimes = CruftMtimes::parse(
            &filesystem
                .read(&cruft.pack_path.with_extension("mtimes"))
                .unwrap(),
            &index,
        )
        .unwrap();
        assert_eq!(mtimes.mtime(orphan), Some(0));
        assert!(!filesystem.exists(&old.pack_path).unwrap());
        assert!(
            !filesystem
                .exists(&old.pack_path.with_extension("rev"))
                .unwrap()
        );
        assert!(repository.read_object(orphan, 1024).is_ok());

        let expired = repository
            .repack(&RepackOptions {
                cruft: true,
                cruft_expire_before: Some(0),
                delete_redundant_packs: true,
                ..RepackOptions::default()
            })
            .unwrap();
        assert!(expired.cruft_pack.is_none());
        assert!(repository.read_object(orphan, 1024).is_err());
        assert!(
            !filesystem
                .exists(&cruft.pack_path.with_extension("mtimes"))
                .unwrap()
        );
    }

    #[test]
    fn redundant_pack_deletion_honors_keep_marker() {
        let (repository, filesystem, reachable, orphan) = fixture();
        let old = repository
            .write_pack(
                &reachable
                    .iter()
                    .copied()
                    .chain([orphan])
                    .collect::<Vec<_>>(),
                &PackOptions::default(),
            )
            .unwrap();
        filesystem
            .write(&old.pack_path.with_extension("keep"), b"protected\n")
            .unwrap();
        let result = repository
            .repack(&RepackOptions {
                cruft: true,
                delete_redundant_packs: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.removed_packs, 0);
        assert!(filesystem.exists(&old.index_path).unwrap());
        assert!(filesystem.exists(&old.pack_path).unwrap());
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
            cruft: false,
            cruft_expire_before: None,
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
    pub cruft_pack: Option<WrittenPack>,
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
        validate_repack_options(options)?;
        let config = self.read_config()?;
        if (options.prune_loose || options.delete_redundant_packs)
            && config.get("extensions.preciousobjects")?.is_some()
            && config.get_bool("extensions.preciousobjects")?
        {
            return Err(Error::InvalidRepository(
                "cannot delete objects in a precious-objects repository".into(),
            ));
        }
        let report = self.fsck(&FsckOptions {
            max_object_size: options.pack.max_object_size,
            max_objects: options.max_objects,
            include_index: options.include_index,
            include_reflogs: options.include_reflogs,
            additional_roots: options.additional_roots.clone(),
        })?;
        let old_packs = self.repack_existing_packs()?;
        let mut ids = report.reachable_objects().to_vec();
        let mut cruft_ids = Vec::new();
        let mut cruft_mtimes = BTreeMap::new();
        if options.include_unreachable {
            ids.extend_from_slice(report.unreachable());
            ids.sort_unstable();
            ids.dedup();
        } else if options.cruft {
            cruft_mtimes = self.repack_object_mtimes(report.unreachable(), &old_packs)?;
            cruft_ids = report
                .unreachable()
                .iter()
                .copied()
                .filter(|id| {
                    options.cruft_expire_before.is_none_or(|expiry| {
                        u64::from(cruft_mtimes.get(id).copied().unwrap_or(0)) > expiry
                    })
                })
                .collect();
        }
        let packed_objects = ids.len() + cruft_ids.len();
        if packed_objects == 0 || options.dry_run {
            return Ok(RepackResult {
                pack: None,
                cruft_pack: None,
                packed_objects,
                pruned_loose_objects: 0,
                removed_packs: 0,
            });
        }

        let written = (!ids.is_empty())
            .then(|| self.write_pack(&ids, &options.pack))
            .transpose()?;
        if let Some(pack) = &written {
            self.verify_repack_count(pack, ids.len(), options.pack.max_object_size)?;
        }
        let cruft_written = (!cruft_ids.is_empty())
            .then(|| self.write_pack(&cruft_ids, &options.pack))
            .transpose()?;
        if let Some(pack) = &cruft_written {
            self.verify_repack_count(pack, cruft_ids.len(), options.pack.max_object_size)?;
            self.publish_cruft_mtimes(pack, &cruft_mtimes)?;
        }

        let pruned_loose_objects = if options.prune_loose {
            self.repack_prune_loose(ids.iter().chain(&cruft_ids).copied())?
        } else {
            0
        };
        let removed_packs = if options.delete_redundant_packs {
            self.repack_remove_old_packs(&old_packs, [&written, &cruft_written])?
        } else {
            0
        };
        Ok(RepackResult {
            pack: written,
            cruft_pack: cruft_written,
            packed_objects,
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

    fn repack_prune_loose(&self, ids: impl Iterator<Item = ObjectId>) -> Result<usize> {
        let mut removed = 0;
        for id in ids {
            let path = self.repack_loose_path(id);
            match self.filesystem().remove_file(&path) {
                Ok(()) => {
                    removed += 1;
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
        Ok(removed)
    }

    fn repack_remove_old_packs(
        &self,
        old_packs: &[(PathBuf, PathBuf)],
        new_packs: [&Option<WrittenPack>; 2],
    ) -> Result<usize> {
        let mut removed = 0;
        for (index, pack) in old_packs {
            if new_packs
                .into_iter()
                .flatten()
                .any(|new| *index == new.index_path && *pack == new.pack_path)
            {
                continue;
            }
            if self.filesystem().exists(&pack.with_extension("keep"))? {
                continue;
            }
            match self.filesystem().remove_file(index) {
                Ok(()) => {}
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
            match self.filesystem().remove_file(pack) {
                Ok(()) => removed += 1,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            for extension in ["mtimes", "rev", "bitmap", "promisor"] {
                match self
                    .filesystem()
                    .remove_file(&pack.with_extension(extension))
                {
                    Ok(()) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(removed)
    }

    fn verify_repack_count(
        &self,
        pack: &WrittenPack,
        expected: usize,
        max_size: usize,
    ) -> Result<()> {
        let verified = self.validate_indexed_pack(&pack.index_path, max_size)?;
        if verified.len() != expected {
            return Err(Error::InvalidObject(
                "published repack object count differs from selection".into(),
            ));
        }
        Ok(())
    }

    fn publish_cruft_mtimes(
        &self,
        pack: &WrittenPack,
        mtimes: &BTreeMap<ObjectId, u32>,
    ) -> Result<()> {
        let index = PackIndex::parse(&self.filesystem().read(&pack.index_path)?)?;
        let encoded = CruftMtimes::encode(&index, mtimes);
        CruftMtimes::parse(&encoded, &index)?;
        let path = pack.pack_path.with_extension("mtimes");
        if self.filesystem().exists(&path)? {
            if self.filesystem().read(&path)? != encoded {
                return Err(Error::AlreadyExists(path));
            }
        } else {
            self.filesystem().write_new(&path, &encoded)?;
        }
        Ok(())
    }

    fn repack_object_mtimes(
        &self,
        ids: &[ObjectId],
        packs: &[(PathBuf, PathBuf)],
    ) -> Result<BTreeMap<ObjectId, u32>> {
        let wanted = ids.iter().copied().collect::<BTreeSet<_>>();
        let mut result = BTreeMap::new();
        for id in ids {
            match self.filesystem().metadata(&self.repack_loose_path(*id)) {
                Ok(metadata) => {
                    result.insert(*id, metadata.stat().mtime_seconds);
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        for (index_path, pack_path) in packs {
            let index = PackIndex::parse(&self.filesystem().read(index_path)?)?;
            let mtimes_path = pack_path.with_extension("mtimes");
            let explicit = match self.filesystem().read(&mtimes_path) {
                Ok(data) => Some(CruftMtimes::parse(&data, &index)?),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            let pack_mtime = self.filesystem().metadata(pack_path)?.stat().mtime_seconds;
            for entry in index.entries() {
                if !wanted.contains(&entry.id) {
                    continue;
                }
                let mtime = explicit
                    .as_ref()
                    .and_then(|mtimes| mtimes.mtime(entry.id))
                    .unwrap_or(pack_mtime);
                result
                    .entry(entry.id)
                    .and_modify(|current| *current = (*current).max(mtime))
                    .or_insert(mtime);
            }
        }
        Ok(result)
    }
}

fn validate_repack_options(options: &RepackOptions) -> Result<()> {
    if options.include_unreachable && options.cruft {
        return Err(Error::InvalidRepository(
            "include_unreachable and cruft are mutually exclusive".into(),
        ));
    }
    if options.cruft_expire_before.is_some() && !options.cruft {
        return Err(Error::InvalidRepository(
            "cruft expiration requires cruft=true".into(),
        ));
    }
    if options.delete_redundant_packs && !options.include_unreachable && !options.cruft {
        return Err(Error::InvalidRepository(
            "deleting old packs requires include_unreachable=true or cruft=true".into(),
        ));
    }
    Ok(())
}
