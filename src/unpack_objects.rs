//! Validate a pack stream and expand its objects into loose storage.

use crate::{
    AnnotatedTag, Commit, EntryMode, Error, IncomingPackOptions, ObjectId, ObjectKind, Repository,
    Result, Tree,
};

/// Validation, mutation, and resource policy for unpacking a pack stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnpackObjectsOptions {
    pub incoming: IncomingPackOptions,
    /// Validate completely without publishing loose objects.
    pub dry_run: bool,
    /// Validate structured objects and their direct links before publication.
    pub strict: bool,
}

/// Objects discovered, skipped, and published by an unpack operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnpackObjectsReport {
    pub objects: Vec<ObjectId>,
    pub written: Vec<ObjectId>,
    pub existing: Vec<ObjectId>,
}

impl Repository {
    /// Validate a complete pack byte stream and optionally publish loose objects.
    ///
    /// Thin REF deltas may resolve against objects already in this repository.
    /// Publication uses the configured filesystem and Git's normal loose-object
    /// fanout. Existing loose or packed objects are reported but not rewritten.
    ///
    /// # Errors
    /// Returns an error for malformed/corrupt packs, unresolved or invalid
    /// deltas, exceeded resource limits, strict object/link failures, or storage
    /// publication failures.
    pub fn unpack_objects(
        &self,
        pack: &[u8],
        options: &UnpackObjectsOptions,
    ) -> Result<UnpackObjectsReport> {
        let validated = self.validate_incoming_pack(pack, &options.incoming)?;
        if options.strict {
            validate_strict(self, &validated, options.incoming.max_object_size)?;
        }
        let objects = validated.object_ids().collect::<Vec<_>>();
        let mut written = Vec::new();
        let mut existing = Vec::new();
        for id in objects.iter().copied() {
            if self.contains_object(id)? {
                existing.push(id);
                continue;
            }
            if !options.dry_run {
                let (kind, data) = validated
                    .object(id)
                    .ok_or_else(|| Error::InvalidObject("validated object disappeared".into()))?;
                let actual = self.write_object(kind, data)?;
                if actual != id {
                    return Err(Error::InvalidObject(
                        "unpacked object ID changed during publication".into(),
                    ));
                }
                written.push(id);
            }
        }
        Ok(UnpackObjectsReport {
            objects,
            written,
            existing,
        })
    }
}

fn validate_strict(
    repository: &Repository,
    incoming: &crate::ValidatedPack,
    max_size: usize,
) -> Result<()> {
    for id in incoming.object_ids() {
        let (kind, data) = incoming
            .object(id)
            .ok_or_else(|| Error::InvalidObject("validated object disappeared".into()))?;
        match kind {
            ObjectKind::Blob => {}
            ObjectKind::Commit => {
                let commit = Commit::parse(data)?;
                require_kind(
                    repository,
                    incoming,
                    commit.tree(),
                    ObjectKind::Tree,
                    max_size,
                )?;
                for parent in commit.parents() {
                    require_kind(repository, incoming, *parent, ObjectKind::Commit, max_size)?;
                }
            }
            ObjectKind::Tree => {
                let tree = Tree::parse(data)?;
                for entry in tree.entries() {
                    if entry.mode() != EntryMode::Gitlink {
                        require_kind(
                            repository,
                            incoming,
                            entry.id(),
                            entry.mode().object_kind(),
                            max_size,
                        )?;
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = AnnotatedTag::parse(data)?;
                require_kind(
                    repository,
                    incoming,
                    tag.target(),
                    tag.target_kind(),
                    max_size,
                )?;
            }
        }
    }
    Ok(())
}

fn require_kind(
    repository: &Repository,
    incoming: &crate::ValidatedPack,
    id: ObjectId,
    expected: ObjectKind,
    max_size: usize,
) -> Result<()> {
    let actual = incoming.object(id).map(|(kind, _)| kind).map_or_else(
        || {
            repository
                .read_object_raw(id, max_size)
                .map(|object| object.kind())
        },
        Ok,
    )?;
    if actual != expected {
        return Err(Error::InvalidObject(format!(
            "object {id} has type {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, ObjectKind, PackOptions, Repository,
        Signature,
    };

    use super::UnpackObjectsOptions;

    #[test]
    fn dry_run_validates_without_writing_and_unpack_publishes_loose_objects() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let first = source
            .write_object(ObjectKind::Blob, &[b'a'; 4096])
            .unwrap();
        let mut changed = vec![b'a'; 4096];
        changed[2000..2004].copy_from_slice(b"rust");
        let second = source.write_object(ObjectKind::Blob, &changed).unwrap();
        let bundle = source
            .build_pack(&[first, second], &PackOptions::default())
            .unwrap();
        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions::default(),
        )
        .unwrap();

        let dry = destination
            .unpack_objects(
                bundle.pack(),
                &UnpackObjectsOptions {
                    dry_run: true,
                    ..UnpackObjectsOptions::default()
                },
            )
            .unwrap();
        assert!(dry.written.is_empty());
        assert!(!destination.contains_object(first).unwrap());
        let report = destination
            .unpack_objects(bundle.pack(), &UnpackObjectsOptions::default())
            .unwrap();
        assert_eq!(report.written.len(), 2);
        assert_eq!(
            destination.read_object(second, 8192).unwrap().data(),
            changed
        );
        let repeated = destination
            .unpack_objects(bundle.pack(), &UnpackObjectsOptions::default())
            .unwrap();
        assert!(repeated.written.is_empty());
        assert_eq!(repeated.existing.len(), 2);
    }

    #[test]
    fn strict_mode_rejects_missing_links_before_publication() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let missing_tree = crate::ObjectId::compute(ObjectKind::Tree, b"");
        let commit = CommitBuilder::new(
            missing_tree,
            Signature::new("A", "a@example.com", 1, 0).unwrap(),
            Signature::new("C", "c@example.com", 1, 0).unwrap(),
        )
        .message(b"broken\n".to_vec())
        .build()
        .encode();
        let id = source.write_object(ObjectKind::Commit, &commit).unwrap();
        let bundle = source.build_pack(&[id], &PackOptions::default()).unwrap();
        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions::default(),
        )
        .unwrap();
        let result = destination.unpack_objects(
            bundle.pack(),
            &UnpackObjectsOptions {
                strict: true,
                ..UnpackObjectsOptions::default()
            },
        );
        assert!(result.is_err());
        assert!(!destination.contains_object(id).unwrap());
    }
}
