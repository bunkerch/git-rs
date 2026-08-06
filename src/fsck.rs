//! Bounded repository object and connectivity verification.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use crate::{
    AnnotatedTag, EntryMode, Error, ObjectId, ObjectKind, ReferenceTarget, Repository, Result, Tree,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsckOptions {
    pub max_object_size: usize,
    pub max_objects: usize,
    pub include_index: bool,
    pub include_reflogs: bool,
    pub additional_roots: Vec<ObjectId>,
}

impl Default for FsckOptions {
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024 * 1024,
            max_objects: 10_000_000,
            include_index: true,
            include_reflogs: true,
            additional_roots: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsckReport {
    pub objects: usize,
    pub reachable: usize,
    pub blobs: usize,
    pub trees: usize,
    pub commits: usize,
    pub tags: usize,
    reachable_objects: Vec<ObjectId>,
    packed_objects: Vec<ObjectId>,
    unreachable: Vec<ObjectId>,
    dangling: Vec<ObjectId>,
}

impl FsckReport {
    #[must_use]
    pub fn reachable_objects(&self) -> &[ObjectId] {
        &self.reachable_objects
    }

    #[must_use]
    pub fn packed_objects(&self) -> &[ObjectId] {
        &self.packed_objects
    }

    #[must_use]
    pub fn unreachable(&self) -> &[ObjectId] {
        &self.unreachable
    }
    #[must_use]
    pub fn dangling(&self) -> &[ObjectId] {
        &self.dangling
    }
}

#[derive(Clone, Copy)]
struct Link {
    id: ObjectId,
    expected: ObjectKind,
}

impl Repository {
    /// Verify all loose and indexed packed objects and their typed links.
    ///
    /// References, reflogs, index entries, and caller-supplied roots determine
    /// reachability. Unreachable and dangling objects are reported, not errors.
    ///
    /// # Errors
    /// Returns an error for corrupt objects or packs, broken or wrongly typed
    /// links, invalid roots, storage failures, or configured resource limits.
    pub fn fsck(&self, options: &FsckOptions) -> Result<FsckReport> {
        let (ids, packed_objects) =
            self.fsck_object_ids(options.max_objects, options.max_object_size)?;
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_objects,
            max_object_size: options.max_object_size,
        })?;
        let mut links = BTreeMap::<ObjectId, Vec<Link>>::new();
        let mut kinds = BTreeMap::<ObjectId, ObjectKind>::new();
        let mut counts = [0_usize; 4];
        for id in &ids {
            let object = self.read_object_raw(*id, options.max_object_size)?;
            let kind = object.kind();
            counts[kind_slot(kind)] += 1;
            let targets = match kind {
                ObjectKind::Blob => Vec::new(),
                ObjectKind::Commit => {
                    let commit = crate::Commit::parse(object.data())?;
                    let mut found = vec![Link {
                        id: commit.tree(),
                        expected: ObjectKind::Tree,
                    }];
                    if !shallow.contains(id) {
                        found.extend(commit.parents().iter().copied().map(|id| Link {
                            id,
                            expected: ObjectKind::Commit,
                        }));
                    }
                    found
                }
                ObjectKind::Tree => Tree::parse(object.data())?
                    .entries()
                    .iter()
                    .filter(|entry| entry.mode() != EntryMode::Gitlink)
                    .map(|entry| Link {
                        id: entry.id(),
                        expected: entry.mode().object_kind(),
                    })
                    .collect(),
                ObjectKind::Tag => {
                    let tag = AnnotatedTag::parse(object.data())?;
                    vec![Link {
                        id: tag.target(),
                        expected: tag.target_kind(),
                    }]
                }
            };
            kinds.insert(*id, kind);
            links.insert(*id, targets);
        }
        for (source, targets) in &links {
            for target in targets {
                let Some(actual) = kinds.get(&target.id) else {
                    return Err(Error::InvalidObject(format!(
                        "broken link from {source} to {}",
                        target.id
                    )));
                };
                if *actual != target.expected {
                    return Err(Error::InvalidObject(format!(
                        "wrong object type in link from {source} to {}: expected {:?}, found {:?}",
                        target.id, target.expected, actual
                    )));
                }
            }
        }

        let mut reachable = BTreeSet::new();
        let mut stack = self.fsck_roots(options)?.into_iter().collect::<Vec<_>>();
        while let Some(id) = stack.pop() {
            if id.is_null() || !reachable.insert(id) {
                continue;
            }
            let Some(targets) = links.get(&id) else {
                return Err(Error::InvalidObject(format!(
                    "retention root {id} is missing"
                )));
            };
            stack.extend(targets.iter().map(|link| link.id));
        }
        let unreachable = ids.difference(&reachable).copied().collect::<Vec<_>>();
        let used = links
            .values()
            .flat_map(|targets| targets.iter().map(|link| link.id))
            .collect::<BTreeSet<_>>();
        let dangling = unreachable
            .iter()
            .copied()
            .filter(|id| !used.contains(id))
            .collect();
        Ok(FsckReport {
            objects: ids.len(),
            reachable: reachable.len(),
            blobs: counts[0],
            trees: counts[1],
            commits: counts[2],
            tags: counts[3],
            reachable_objects: reachable.iter().copied().collect(),
            packed_objects: packed_objects.into_iter().collect(),
            unreachable,
            dangling,
        })
    }

    fn fsck_object_ids(
        &self,
        limit: usize,
        max_object_size: usize,
    ) -> Result<(BTreeSet<ObjectId>, BTreeSet<ObjectId>)> {
        let mut ids = BTreeSet::new();
        let mut packed = BTreeSet::new();
        let objects = self.git_path("objects");
        for directory in self.filesystem().read_dir(&objects)? {
            let Some(fanout) = directory.to_str() else {
                continue;
            };
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            for file in self.filesystem().read_dir(&objects.join(&directory))? {
                let Some(suffix) = file.to_str() else {
                    continue;
                };
                if suffix.len() == 38 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    insert_bounded(
                        &mut ids,
                        ObjectId::from_str(&format!("{fanout}{suffix}"))?,
                        limit,
                    )?;
                }
            }
        }
        let packs = objects.join("pack");
        match self.filesystem().read_dir(&packs) {
            Ok(files) => {
                for file in files {
                    if file.extension().and_then(|value| value.to_str()) != Some("idx") {
                        continue;
                    }
                    for id in self.validate_indexed_pack(&packs.join(file), max_object_size)? {
                        insert_bounded(&mut ids, id, limit)?;
                        packed.insert(id);
                    }
                }
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok((ids, packed))
    }

    fn fsck_roots(&self, options: &FsckOptions) -> Result<BTreeSet<ObjectId>> {
        let references = self.references()?;
        let mut roots = options
            .additional_roots
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for reference in &references {
            roots.insert(match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(reference.name())?,
            });
        }
        match self.resolve_reference("HEAD") {
            Ok(id) => {
                roots.insert(id);
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        if options.include_index {
            roots.extend(
                self.read_index()?
                    .entries()
                    .iter()
                    .filter(|entry| entry.mode() != 0o160_000)
                    .map(crate::IndexEntry::id),
            );
        }
        if options.include_reflogs {
            for name in std::iter::once("HEAD").chain(references.iter().map(crate::Reference::name))
            {
                for entry in self.read_reflog(name)? {
                    if !entry.old_id().is_null() {
                        roots.insert(entry.old_id());
                    }
                    if !entry.new_id().is_null() {
                        roots.insert(entry.new_id());
                    }
                }
            }
        }
        Ok(roots)
    }
}

fn insert_bounded(ids: &mut BTreeSet<ObjectId>, id: ObjectId, limit: usize) -> Result<()> {
    ids.insert(id);
    if ids.len() > limit {
        return Err(Error::InvalidRepository(
            "fsck object count limit exceeded".into(),
        ));
    }
    Ok(())
}

const fn kind_slot(kind: ObjectKind) -> usize {
    match kind {
        ObjectKind::Blob => 0,
        ObjectKind::Tree => 1,
        ObjectKind::Commit => 2,
        ObjectKind::Tag => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitOptions, FileSystem, InitOptions, MemoryFileSystem, PackOptions, Signature, TreeEntry,
    };
    use std::path::Path;

    fn fixture() -> (Repository, MemoryFileSystem, ObjectId, ObjectId, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/file"), b"content")
            .unwrap();
        repository.add("file").unwrap();
        let blob = repository.read_index().unwrap().entries()[0].id();
        let signature = Signature::new("Fsck", "fsck@example.com", 100, 0).unwrap();
        let commit = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        let tree = repository.read_commit(commit, 1024).unwrap().tree();
        (repository, filesystem, commit, tree, blob)
    }

    fn loose_path(id: ObjectId) -> String {
        let hex = String::from_utf8(id.to_hex().to_vec()).unwrap();
        format!("repo/.git/objects/{}/{}", &hex[..2], &hex[2..])
    }

    #[test]
    fn reports_counts_unreachable_and_dangling_objects() {
        let (repository, _, _, _, _) = fixture();
        let orphan = repository
            .write_object(ObjectKind::Blob, b"orphan")
            .unwrap();
        let report = repository.fsck(&FsckOptions::default()).unwrap();
        assert_eq!(report.objects, 4);
        assert_eq!(report.reachable, 3);
        assert_eq!(report.blobs, 2);
        assert_eq!(report.trees, 1);
        assert_eq!(report.commits, 1);
        assert_eq!(report.tags, 0);
        assert_eq!(report.unreachable(), &[orphan]);
        assert_eq!(report.dangling(), &[orphan]);
    }

    #[test]
    fn validates_packed_only_objects_and_caller_roots() {
        let (repository, filesystem, commit, tree, blob) = fixture();
        let orphan = repository.write_object(ObjectKind::Blob, b"held").unwrap();
        repository
            .write_pack(&[commit, tree, blob, orphan], &PackOptions::default())
            .unwrap();
        for id in [commit, tree, blob, orphan] {
            filesystem.remove_file(Path::new(&loose_path(id))).unwrap();
        }
        let report = repository
            .fsck(&FsckOptions {
                additional_roots: vec![orphan],
                ..FsckOptions::default()
            })
            .unwrap();
        assert_eq!(report.objects, 4);
        assert_eq!(report.reachable, 4);
        assert!(report.unreachable().is_empty());
    }

    #[test]
    fn validates_each_pack_even_when_objects_have_valid_duplicates() {
        let (repository, filesystem, commit, tree, blob) = fixture();
        let first = repository
            .write_pack(&[commit, tree, blob], &PackOptions::default())
            .unwrap();
        let extra = repository.write_object(ObjectKind::Blob, b"extra").unwrap();
        repository
            .write_pack(&[commit, tree, blob, extra], &PackOptions::default())
            .unwrap();
        filesystem.write(&first.pack_path, b"broken pack").unwrap();
        assert!(matches!(
            repository.fsck(&FsckOptions::default()),
            Err(Error::InvalidObject(_))
        ));
    }

    #[test]
    fn rejects_corruption_and_missing_links() {
        let (repository, filesystem, _, tree, _) = fixture();
        filesystem
            .write(Path::new(&loose_path(tree)), b"not zlib")
            .unwrap();
        assert!(matches!(
            repository.fsck(&FsckOptions::default()),
            Err(Error::Compression(_))
        ));

        let (repository, filesystem, _, tree, _) = fixture();
        filesystem
            .remove_file(Path::new(&loose_path(tree)))
            .unwrap();
        let error = repository.fsck(&FsckOptions::default()).unwrap_err();
        assert!(error.to_string().contains("broken link"));
    }

    #[test]
    fn rejects_wrong_typed_tree_links_and_enforces_object_limit() {
        let (repository, _, _, _, blob) = fixture();
        let invalid =
            Tree::new(vec![TreeEntry::new(EntryMode::Tree, b"dir", blob).unwrap()]).unwrap();
        let invalid_id = repository.write_tree(&invalid).unwrap();
        let error = repository
            .fsck(&FsckOptions {
                additional_roots: vec![invalid_id],
                ..FsckOptions::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("wrong object type"));
        assert!(
            repository
                .fsck(&FsckOptions {
                    max_objects: 1,
                    ..FsckOptions::default()
                })
                .is_err()
        );
    }
}
