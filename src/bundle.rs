//! Git bundle v2/v3 parsing, creation, verification, and import.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;

use crate::{
    AnnotatedTag, EntryMode, Error, IncomingPackOptions, ObjectId, ObjectKind, PackOptions,
    ReferenceName, Repository, Result, Tree, ValidatedPack, WrittenPack,
};

const V2_SIGNATURE: &[u8] = b"# v2 git bundle\n";
const V3_SIGNATURE: &[u8] = b"# v3 git bundle\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleVersion {
    V2,
    V3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleReference {
    id: ObjectId,
    name: String,
}

impl BundleReference {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundlePrerequisite {
    id: ObjectId,
    subject: Vec<u8>,
}

impl BundlePrerequisite {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub fn subject(&self) -> &[u8] {
        &self.subject
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleParseOptions {
    pub max_header_size: usize,
    pub max_references: usize,
    pub max_prerequisites: usize,
    pub max_pack_size: usize,
}

impl Default for BundleParseOptions {
    fn default() -> Self {
        Self {
            max_header_size: 16 * 1024 * 1024,
            max_references: 1_000_000,
            max_prerequisites: 1_000_000,
            max_pack_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleCreateOptions {
    pub version: BundleVersion,
    pub references: Vec<String>,
    pub prerequisites: Vec<ObjectId>,
    pub max_object_size: usize,
    pub max_objects: usize,
    pub use_deltas: bool,
}

impl Default for BundleCreateOptions {
    fn default() -> Self {
        Self {
            version: BundleVersion::V2,
            references: Vec::new(),
            prerequisites: Vec::new(),
            max_object_size: 1024 * 1024 * 1024,
            max_objects: 10_000_000,
            use_deltas: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitBundle {
    version: BundleVersion,
    prerequisites: Vec<BundlePrerequisite>,
    references: Vec<BundleReference>,
    pack: Vec<u8>,
}

impl GitBundle {
    /// Parse a complete bundle without accessing a repository.
    ///
    /// Version 3 accepts the `object-format=sha1`
    /// capability. Filtered bundles are rejected because treating their
    /// intentionally incomplete graph as complete would be unsafe.
    ///
    /// # Errors
    /// Returns an error for malformed signatures/headers, unsupported or
    /// duplicate capabilities, invalid IDs/ref names, missing pack data, or
    /// exceeded resource limits.
    pub fn parse(data: &[u8], options: &BundleParseOptions) -> Result<Self> {
        if data.len()
            > options
                .max_header_size
                .saturating_add(options.max_pack_size)
        {
            return bundle_error("bundle exceeds combined size limits");
        }
        let (version, mut cursor) = if data.starts_with(V2_SIGNATURE) {
            (BundleVersion::V2, V2_SIGNATURE.len())
        } else if data.starts_with(V3_SIGNATURE) {
            (BundleVersion::V3, V3_SIGNATURE.len())
        } else {
            return bundle_error("missing v2 or v3 signature");
        };
        let mut prerequisites = Vec::new();
        let mut references = Vec::new();
        let mut prerequisite_ids = BTreeSet::new();
        let mut reference_names = BTreeSet::new();
        let mut object_format_seen = false;
        loop {
            if cursor > options.max_header_size {
                return bundle_error("bundle header exceeds size limit");
            }
            let newline = data[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|offset| cursor + offset)
                .ok_or_else(|| Error::Protocol("unterminated bundle header".into()))?;
            if newline >= options.max_header_size {
                return bundle_error("bundle header exceeds size limit");
            }
            let mut line = &data[cursor..newline];
            cursor = newline + 1;
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() {
                break;
            }
            if let Some(capability) = line.strip_prefix(b"@") {
                if version != BundleVersion::V3 {
                    return bundle_error("capability in a v2 bundle");
                }
                if capability == b"object-format=sha1" && !object_format_seen {
                    object_format_seen = true;
                    continue;
                }
                if capability.starts_with(b"filter=") {
                    return bundle_error("filtered bundles are unsupported");
                }
                return bundle_error("unknown or duplicate bundle capability");
            }
            if let Some(prerequisite) = line.strip_prefix(b"-") {
                if prerequisites.len() >= options.max_prerequisites {
                    return bundle_error("bundle exceeds prerequisite limit");
                }
                let (id, subject) = parse_header_object(prerequisite, false)?;
                if !prerequisite_ids.insert(id) {
                    return bundle_error("duplicate bundle prerequisite");
                }
                prerequisites.push(BundlePrerequisite {
                    id,
                    subject: subject.to_vec(),
                });
            } else {
                if references.len() >= options.max_references {
                    return bundle_error("bundle exceeds reference limit");
                }
                let (id, name) = parse_header_object(line, true)?;
                let name = std::str::from_utf8(name)
                    .map_err(|_| Error::InvalidReference("bundle ref is not UTF-8".into()))?;
                validate_bundle_ref(name)?;
                if !reference_names.insert(name.to_owned()) {
                    return bundle_error("duplicate bundle reference");
                }
                references.push(BundleReference {
                    id,
                    name: name.to_owned(),
                });
            }
        }
        if references.is_empty() {
            return bundle_error("bundle has no references");
        }
        let pack = data
            .get(cursor..)
            .ok_or_else(|| Error::Protocol("missing bundle pack".into()))?;
        if pack.len() > options.max_pack_size || !pack.starts_with(b"PACK") {
            return bundle_error("missing or oversized bundle pack");
        }
        Ok(Self {
            version,
            prerequisites,
            references,
            pack: pack.to_vec(),
        })
    }

    #[must_use]
    pub const fn version(&self) -> BundleVersion {
        self.version
    }

    #[must_use]
    pub fn prerequisites(&self) -> &[BundlePrerequisite] {
        &self.prerequisites
    }

    #[must_use]
    pub fn references(&self) -> &[BundleReference] {
        &self.references
    }

    #[must_use]
    pub fn pack(&self) -> &[u8] {
        &self.pack
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = match self.version {
            BundleVersion::V2 => V2_SIGNATURE.to_vec(),
            BundleVersion::V3 => {
                let mut header = V3_SIGNATURE.to_vec();
                header.extend_from_slice(b"@object-format=sha1\n");
                header
            }
        };
        for prerequisite in &self.prerequisites {
            output.extend_from_slice(format!("-{}", prerequisite.id).as_bytes());
            if !prerequisite.subject.is_empty() {
                output.push(b' ');
                output.extend_from_slice(&prerequisite.subject);
            }
            output.push(b'\n');
        }
        for reference in &self.references {
            output.extend_from_slice(format!("{} {}\n", reference.id, reference.name).as_bytes());
        }
        output.push(b'\n');
        output.extend_from_slice(&self.pack);
        output
    }
}

impl Repository {
    /// Create a deterministic bundle from named refs and optional prerequisite
    /// commits. Objects reachable from prerequisites are excluded from its pack.
    ///
    /// # Errors
    /// Returns an error for no refs, duplicate/invalid refs or prerequisites,
    /// non-commit prerequisites, missing/corrupt graphs, exceeded limits, or
    /// pack construction failure.
    pub fn create_bundle(&self, options: &BundleCreateOptions) -> Result<GitBundle> {
        if options.references.is_empty() {
            return bundle_error("refusing to create an empty bundle");
        }
        let mut names = BTreeSet::new();
        let mut references = Vec::with_capacity(options.references.len());
        for name in &options.references {
            validate_bundle_ref(name)?;
            if !names.insert(name.clone()) {
                return bundle_error("duplicate bundle reference");
            }
            references.push(BundleReference {
                id: self.resolve_reference(name)?,
                name: name.clone(),
            });
        }
        let mut prerequisites = Vec::with_capacity(options.prerequisites.len());
        let mut prerequisite_ids = BTreeSet::new();
        for id in &options.prerequisites {
            if !prerequisite_ids.insert(*id) {
                return bundle_error("duplicate bundle prerequisite");
            }
            let commit = self.read_commit(*id, options.max_object_size)?;
            let subject = commit
                .message()
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default()
                .iter()
                .map(|byte| {
                    if matches!(byte, b'\r' | 0) {
                        b' '
                    } else {
                        *byte
                    }
                })
                .collect();
            prerequisites.push(BundlePrerequisite { id: *id, subject });
        }
        let roots = references
            .iter()
            .map(BundleReference::id)
            .collect::<Vec<_>>();
        let wanted = self.reachable_objects_bounded(
            &roots,
            options.max_object_size,
            false,
            options.max_objects,
        )?;
        let excluded = self
            .reachable_objects_bounded(
                &options.prerequisites,
                options.max_object_size,
                false,
                options.max_objects,
            )?
            .into_iter()
            .collect::<HashSet<_>>();
        let ids = wanted
            .into_iter()
            .filter(|id| !excluded.contains(id))
            .collect::<Vec<_>>();
        let pack = self.build_pack(
            &ids,
            &PackOptions {
                max_object_size: options.max_object_size,
                use_deltas: options.use_deltas,
            },
        )?;
        Ok(GitBundle {
            version: options.version,
            prerequisites,
            references,
            pack: pack.pack().to_vec(),
        })
    }

    /// Validate prerequisites, incoming pack integrity, advertised tips, and
    /// the complete typed object graph without publishing anything.
    ///
    /// # Errors
    /// Returns an error for missing/unconnected prerequisites, invalid packs,
    /// absent tips, broken or wrongly typed links, or resource-limit failures.
    pub fn verify_bundle(
        &self,
        bundle: &GitBundle,
        options: &IncomingPackOptions,
        max_objects: usize,
    ) -> Result<ValidatedPack> {
        for prerequisite in &bundle.prerequisites {
            let object = self.read_object(prerequisite.id, options.max_object_size)?;
            if object.kind() != ObjectKind::Commit {
                return bundle_error("bundle prerequisite is not a commit");
            }
            self.reachable_objects_bounded(
                &[prerequisite.id],
                options.max_object_size,
                false,
                max_objects,
            )?;
        }
        let validated = self.validate_incoming_pack(&bundle.pack, options)?;
        verify_bundle_graph(
            self,
            bundle,
            &validated,
            options.max_object_size,
            max_objects,
        )?;
        Ok(validated)
    }

    /// Verify and publish a bundle pack. Like `git bundle unbundle`, this does
    /// not update refs; callers choose mappings from [`GitBundle::references`].
    ///
    /// # Errors
    /// Returns verification errors or atomic pack publication failures.
    pub fn unbundle(
        &self,
        bundle: &GitBundle,
        options: &IncomingPackOptions,
        max_objects: usize,
    ) -> Result<WrittenPack> {
        let validated = self.verify_bundle(bundle, options, max_objects)?;
        self.publish_validated_pack(&validated)
    }
}

fn verify_bundle_graph(
    repository: &Repository,
    bundle: &GitBundle,
    pack: &ValidatedPack,
    max_size: usize,
    max_objects: usize,
) -> Result<()> {
    let prerequisites = bundle
        .prerequisites
        .iter()
        .map(BundlePrerequisite::id)
        .collect::<HashSet<_>>();
    let mut kinds = BTreeMap::new();
    let mut stack = bundle
        .references
        .iter()
        .map(|reference| (reference.id(), None))
        .collect::<Vec<_>>();
    while let Some((id, expected)) = stack.pop() {
        if prerequisites.contains(&id) {
            if expected.is_some_and(|kind| kind != ObjectKind::Commit) {
                return bundle_error(format!("prerequisite {id} is used as a non-commit object"));
            }
            continue;
        }
        if let Some(kind) = kinds.get(&id) {
            if expected.is_some_and(|expected| expected != *kind) {
                return bundle_error(format!(
                    "object {id} has kind {kind:?}, expected {expected:?}"
                ));
            }
            continue;
        }
        if kinds.len() >= max_objects {
            return bundle_error("bundle graph exceeds object limit");
        }
        let owned;
        let (kind, data) = if let Some(value) = pack.object(id) {
            value
        } else {
            owned = repository.read_object(id, max_size)?;
            (owned.kind(), owned.data())
        };
        if expected.is_some_and(|expected| expected != kind) {
            return bundle_error(format!(
                "object {id} has kind {kind:?}, expected {expected:?}"
            ));
        }
        kinds.insert(id, kind);
        match kind {
            ObjectKind::Blob => {}
            ObjectKind::Commit => {
                let commit = crate::Commit::parse(data)?;
                stack.push((commit.tree(), Some(ObjectKind::Tree)));
                stack.extend(
                    commit
                        .parents()
                        .iter()
                        .copied()
                        .map(|parent| (parent, Some(ObjectKind::Commit))),
                );
            }
            ObjectKind::Tree => {
                let tree = Tree::parse(data)?;
                stack.extend(
                    tree.entries()
                        .iter()
                        .filter(|entry| entry.mode() != EntryMode::Gitlink)
                        .map(|entry| (entry.id(), Some(entry.mode().object_kind()))),
                );
            }
            ObjectKind::Tag => {
                let tag = AnnotatedTag::parse(data)?;
                stack.push((tag.target(), Some(tag.target_kind())));
            }
        }
    }
    Ok(())
}

fn parse_header_object(line: &[u8], require_name: bool) -> Result<(ObjectId, &[u8])> {
    if line.len() < ObjectId::HEX_LENGTH {
        return bundle_error("truncated object ID in bundle header");
    }
    let id = ObjectId::from_str(
        std::str::from_utf8(&line[..ObjectId::HEX_LENGTH])
            .map_err(|_| Error::InvalidObjectId("non-UTF-8 bundle ID".into()))?,
    )?;
    let remainder = &line[ObjectId::HEX_LENGTH..];
    if remainder.is_empty() {
        if require_name {
            return bundle_error("bundle reference has no name");
        }
        return Ok((id, b""));
    }
    if remainder[0] != b' ' {
        return bundle_error("bundle object ID is not followed by a space");
    }
    if require_name && remainder.len() == 1 {
        return bundle_error("bundle reference has an empty name");
    }
    Ok((id, &remainder[1..]))
}

fn validate_bundle_ref(name: &str) -> Result<()> {
    if name == "HEAD" {
        return Ok(());
    }
    ReferenceName::new(name.to_owned()).map(|_| ())
}

fn bundle_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(format!(
        "invalid Git bundle: {}",
        message.into()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, PreviousValue, Signature, TreeEntry,
    };

    fn fixture() -> (Repository, ObjectId, ObjectId) {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("A U Thor", "author@example.com", 1, 0).unwrap();
        let first_blob = repository.write_object(ObjectKind::Blob, b"first").unwrap();
        let first_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), first_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let first = repository
            .write_commit(
                &CommitBuilder::new(first_tree, signature.clone(), signature.clone())
                    .message(b"first\n")
                    .build(),
            )
            .unwrap();
        let second_blob = repository
            .write_object(ObjectKind::Blob, b"second")
            .unwrap();
        let second_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), second_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let second = repository
            .write_commit(
                &CommitBuilder::new(second_tree, signature.clone(), signature)
                    .parent(first)
                    .message(b"second\n")
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                second,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        (repository, first, second)
    }

    #[test]
    fn creates_parses_and_unbundles_incremental_history() {
        let (source, first, second) = fixture();
        let bundle = source
            .create_bundle(&BundleCreateOptions {
                references: vec!["refs/heads/main".into()],
                prerequisites: vec![first],
                ..BundleCreateOptions::default()
            })
            .unwrap();
        assert_eq!(bundle.references()[0].id(), second);
        assert_eq!(bundle.prerequisites()[0].subject(), b"first");
        let parsed = GitBundle::parse(&bundle.encode(), &BundleParseOptions::default()).unwrap();
        assert_eq!(parsed, bundle);

        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions::default(),
        )
        .unwrap();
        for id in source
            .reachable_objects(&[first], usize::MAX, false)
            .unwrap()
        {
            let object = source.read_object(id, usize::MAX).unwrap();
            assert_eq!(
                destination
                    .write_object(object.kind(), object.data())
                    .unwrap(),
                id
            );
        }
        destination
            .unbundle(&parsed, &IncomingPackOptions::default(), 100)
            .unwrap();
        assert_eq!(
            destination.read_commit(second, 1024).unwrap().parents(),
            &[first]
        );
    }

    #[test]
    fn rejects_missing_prerequisites_caps_duplicate_refs_and_bad_pack() {
        let (source, first, _) = fixture();
        let bundle = source
            .create_bundle(&BundleCreateOptions {
                version: BundleVersion::V3,
                references: vec!["refs/heads/main".into()],
                prerequisites: vec![first],
                ..BundleCreateOptions::default()
            })
            .unwrap();
        let parsed = GitBundle::parse(&bundle.encode(), &BundleParseOptions::default()).unwrap();
        let empty =
            Repository::init(MemoryFileSystem::new(), "empty", &InitOptions::default()).unwrap();
        assert!(
            empty
                .verify_bundle(&parsed, &IncomingPackOptions::default(), 100)
                .is_err()
        );
        let duplicate =
            format!("# v2 git bundle\n{first} refs/heads/a\n{first} refs/heads/a\n\nPACK");
        assert!(GitBundle::parse(duplicate.as_bytes(), &BundleParseOptions::default()).is_err());
        assert!(
            GitBundle::parse(
                b"# v3 git bundle\n@filter=blob:none\n\nPACK",
                &BundleParseOptions::default()
            )
            .is_err()
        );
    }
}
