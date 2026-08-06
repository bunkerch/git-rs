//! Annotated tag objects and tag reference operations.

use std::collections::HashSet;
use std::str::FromStr;

use crate::{
    Error, ExtraHeader, ObjectId, ObjectKind, PreviousValue, Reference, ReferenceName, Repository,
    Result, Signature,
};

/// Parsed annotated tag object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnotatedTag {
    target: ObjectId,
    target_kind: ObjectKind,
    name: Vec<u8>,
    tagger: Option<Signature>,
    extra_headers: Vec<ExtraHeader>,
    message: Vec<u8>,
}

/// Validation and publication policy for [`Repository::mk_tag`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MkTagOptions {
    /// Promote fsck-style tag warnings to errors, matching `git mktag`.
    pub strict: bool,
    /// Validate and compute the ID without storing the tag object.
    pub dry_run: bool,
    pub max_input_size: usize,
    pub max_target_size: usize,
}

impl Default for MkTagOptions {
    fn default() -> Self {
        Self {
            strict: true,
            dry_run: false,
            max_input_size: 1024 * 1024 * 1024,
            max_target_size: 1024 * 1024 * 1024,
        }
    }
}

impl AnnotatedTag {
    #[must_use]
    pub const fn target(&self) -> ObjectId {
        self.target
    }

    #[must_use]
    pub const fn target_kind(&self) -> ObjectKind {
        self.target_kind
    }

    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    #[must_use]
    pub const fn tagger(&self) -> Option<&Signature> {
        self.tagger.as_ref()
    }

    #[must_use]
    pub fn extra_headers(&self) -> &[ExtraHeader] {
        &self.extra_headers
    }

    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// Parse canonical tag headers while preserving extra headers and message
    /// bytes (including signature blocks).
    ///
    /// # Errors
    /// Returns an error for missing, duplicate, out-of-order, or malformed
    /// required headers, invalid IDs/types/identity, NULs, or continuations.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let (header_end, message_start) = match data.windows(2).position(|window| window == b"\n\n")
        {
            Some(separator) => (separator, separator + 2),
            None if data.ends_with(b"\n") => (data.len() - 1, data.len()),
            None => return Err(Error::InvalidObject("unterminated tag header".into())),
        };
        let headers = parse_headers(&data[..header_end])?;
        if headers.len() < 3
            || headers[0].0 != b"object"
            || headers[1].0 != b"type"
            || headers[2].0 != b"tag"
        {
            return Err(Error::InvalidObject(
                "tag requires object, type, and tag headers in order".into(),
            ));
        }
        let target = parse_id(&headers[0].1)?;
        let target_kind = parse_kind(&headers[1].1)?;
        validate_tag_header_name(&headers[2].1)?;
        let name = headers[2].1.clone();
        let mut tagger = None;
        let mut extra_headers = Vec::new();
        for (index, (name, value)) in headers.into_iter().enumerate().skip(3) {
            if name == b"tagger" {
                if index != 3 || tagger.is_some() {
                    return Err(Error::InvalidObject(
                        "tagger must appear once immediately after tag".into(),
                    ));
                }
                tagger = Some(Signature::parse(&value)?);
            } else if matches!(name.as_slice(), b"object" | b"type" | b"tag") {
                return Err(Error::InvalidObject("duplicate required tag header".into()));
            } else {
                extra_headers.push(ExtraHeader::new(name, value)?);
            }
        }
        Ok(Self {
            target,
            target_kind,
            name,
            tagger,
            extra_headers,
            message: data[message_start..].to_vec(),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut data = format!(
            "object {}\ntype {}\ntag ",
            self.target,
            kind_name(self.target_kind),
        )
        .into_bytes();
        data.extend_from_slice(&self.name);
        data.push(b'\n');
        if let Some(tagger) = &self.tagger {
            data.extend_from_slice(format!("tagger {}\n", tagger.encode()).as_bytes());
        }
        for header in &self.extra_headers {
            data.extend_from_slice(header.name());
            data.push(b' ');
            data.extend_from_slice(header.value());
            data.push(b'\n');
        }
        data.push(b'\n');
        data.extend_from_slice(&self.message);
        data
    }
}

/// Builder for an annotated tag.
#[derive(Clone, Debug)]
pub struct TagBuilder {
    tag: AnnotatedTag,
}

impl TagBuilder {
    /// Create an unsigned annotated tag value.
    ///
    /// # Errors
    /// Returns an error for an empty/unsafe tag header name.
    pub fn new(
        target: ObjectId,
        target_kind: ObjectKind,
        name: impl AsRef<[u8]>,
        tagger: Signature,
    ) -> Result<Self> {
        let name = name.as_ref().to_vec();
        validate_tag_header_name(&name)?;
        Ok(Self {
            tag: AnnotatedTag {
                target,
                target_kind,
                name,
                tagger: Some(tagger),
                extra_headers: Vec::new(),
                message: Vec::new(),
            },
        })
    }

    #[must_use]
    pub fn message(mut self, message: impl Into<Vec<u8>>) -> Self {
        self.tag.message = message.into();
        self
    }

    #[must_use]
    pub fn extra_header(mut self, header: ExtraHeader) -> Self {
        self.tag.extra_headers.push(header);
        self
    }

    #[must_use]
    pub fn build(self) -> AnnotatedTag {
        self.tag
    }
}

/// Final non-tag object reached by peeling nested annotated tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeeledObject {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub depth: usize,
}

impl Repository {
    /// Validate exact annotated-tag bytes and optionally store the tag object.
    ///
    /// Strict mode rejects fsck warnings (missing tagger, invalid ref-style tag
    /// name, or extra headers). Both modes verify the target's declared type.
    ///
    /// # Errors
    /// Returns an error for oversized or malformed input, strict warnings,
    /// missing/corrupt targets, declared-type mismatch, or storage failures.
    pub fn mk_tag(&self, data: &[u8], options: &MkTagOptions) -> Result<ObjectId> {
        if data.len() > options.max_input_size {
            return Err(Error::ObjectTooLarge {
                declared: data.len() as u64,
                limit: options.max_input_size,
            });
        }
        let tag = AnnotatedTag::parse(data)?;
        if options.strict {
            if tag.tagger.is_none() {
                return Err(Error::InvalidObject(
                    "strict tag requires a tagger header".into(),
                ));
            }
            let valid_signature_header = tag.extra_headers.len() == 1
                && matches!(tag.extra_headers[0].name(), b"gpgsig" | b"gpgsig-sha256");
            if !tag.extra_headers.is_empty() && !valid_signature_header {
                return Err(Error::InvalidObject(
                    "strict tag contains extra header entries".into(),
                ));
            }
            let name = std::str::from_utf8(&tag.name)
                .map_err(|_| Error::InvalidObject("strict tag name is not UTF-8".into()))?;
            ReferenceName::new(format!("refs/tags/{name}"))
                .map_err(|_| Error::InvalidObject("invalid strict tag name".into()))?;
        }
        let target = self.read_object_raw(tag.target, options.max_target_size)?;
        if target.kind() != tag.target_kind {
            return Err(Error::InvalidObject(format!(
                "tag declares {} but target is {}",
                kind_name(tag.target_kind),
                kind_name(target.kind())
            )));
        }
        let id = ObjectId::compute(ObjectKind::Tag, data);
        if options.dry_run {
            Ok(id)
        } else {
            self.write_object(ObjectKind::Tag, data)
        }
    }

    /// Store an annotated tag object after verifying its declared target type.
    ///
    /// # Errors
    /// Returns an error for a missing/oversized target, type mismatch, or
    /// object-storage failure.
    pub fn write_tag(&self, tag: &AnnotatedTag, max_object_size: usize) -> Result<ObjectId> {
        let target = self.read_object(tag.target, max_object_size)?;
        if target.kind() != tag.target_kind {
            return Err(Error::InvalidObject(format!(
                "tag declares {} but target is {}",
                kind_name(tag.target_kind),
                kind_name(target.kind())
            )));
        }
        self.write_object(ObjectKind::Tag, &tag.encode())
    }

    /// Read and parse an annotated tag object.
    ///
    /// # Errors
    /// Returns an error for a missing/oversized/non-tag or malformed object.
    pub fn read_tag(&self, id: ObjectId, max_object_size: usize) -> Result<AnnotatedTag> {
        let object = self.read_object(id, max_object_size)?;
        if object.kind() != ObjectKind::Tag {
            return Err(Error::InvalidObject(format!("object {id} is not a tag")));
        }
        AnnotatedTag::parse(object.data())
    }

    /// Create or replace a lightweight tag reference after verifying `target`.
    ///
    /// # Errors
    /// Returns an error for invalid names, missing targets, conflicts, or
    /// storage failures.
    pub fn create_lightweight_tag(
        &self,
        name: &str,
        target: ObjectId,
        force: bool,
        max_object_size: usize,
    ) -> Result<Reference> {
        self.read_object(target, max_object_size)?;
        self.update_tag_reference(name, target, force)
    }

    /// Write an annotated tag object and create or replace its tag reference.
    ///
    /// # Errors
    /// Returns an error for tag validation, target/type mismatch, ref conflict,
    /// or storage failure.
    pub fn create_annotated_tag(
        &self,
        name: &str,
        tag: &AnnotatedTag,
        force: bool,
        max_object_size: usize,
    ) -> Result<(Reference, ObjectId)> {
        if tag.name != name.as_bytes() {
            return Err(Error::InvalidObject(
                "tag header name does not match reference name".into(),
            ));
        }
        let id = self.write_tag(tag, max_object_size)?;
        Ok((self.update_tag_reference(name, id, force)?, id))
    }

    /// List loose and packed tag references in bytewise order.
    ///
    /// # Errors
    /// Returns an error for malformed refs or storage failures.
    pub fn tags(&self) -> Result<Vec<Reference>> {
        Ok(self
            .references()?
            .into_iter()
            .filter(|reference| reference.name().starts_with("refs/tags/"))
            .collect())
    }

    /// Delete a tag using compare-and-swap semantics.
    ///
    /// # Errors
    /// Returns an error for an invalid/missing/stale tag or storage failure.
    pub fn delete_tag(&self, name: &str, expected: ObjectId) -> Result<()> {
        self.delete_reference(&tag_reference_name(name)?, expected)
    }

    /// Peel nested annotated tags to a blob, tree, or commit.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt objects, declared-type mismatch,
    /// cycles, or a nesting depth above `max_depth`.
    pub fn peel_tag(
        &self,
        id: ObjectId,
        max_depth: usize,
        max_object_size: usize,
    ) -> Result<PeeledObject> {
        let mut current = id;
        let mut seen = HashSet::new();
        for depth in 0..=max_depth {
            if !seen.insert(current) {
                return Err(Error::InvalidObject("annotated tag cycle".into()));
            }
            let object = self.read_object(current, max_object_size)?;
            if object.kind() != ObjectKind::Tag {
                return Ok(PeeledObject {
                    id: current,
                    kind: object.kind(),
                    depth,
                });
            }
            if depth == max_depth {
                break;
            }
            let tag = AnnotatedTag::parse(object.data())?;
            let target = self.read_object(tag.target, max_object_size)?;
            if target.kind() != tag.target_kind {
                return Err(Error::InvalidObject("tag target type mismatch".into()));
            }
            current = tag.target;
        }
        Err(Error::InvalidObject(format!(
            "annotated tag depth exceeds {max_depth}"
        )))
    }

    fn update_tag_reference(&self, name: &str, target: ObjectId, force: bool) -> Result<Reference> {
        let name = tag_reference_name(name)?;
        self.update_reference(
            &name,
            target,
            if force {
                PreviousValue::Any
            } else {
                PreviousValue::MustNotExist
            },
        )?;
        self.read_reference(name.as_str())
    }
}

fn tag_reference_name(name: &str) -> Result<ReferenceName> {
    ReferenceName::new(format!("refs/tags/{name}"))
}

fn validate_tag_header_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.contains(&0) || name.contains(&b'\n') || name.contains(&b'\r') {
        return Err(Error::InvalidObject("invalid tag header name".into()));
    }
    Ok(())
}

fn parse_headers(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in data.split(|byte| *byte == b'\n') {
        if line.starts_with(b" ") {
            let (_, value) = headers
                .last_mut()
                .ok_or_else(|| Error::InvalidObject("orphan tag continuation".into()))?;
            value.push(b'\n');
            value.extend_from_slice(line);
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::InvalidObject("tag header has no value".into()))?;
        headers.push((line[..separator].to_vec(), line[separator + 1..].to_vec()));
    }
    Ok(headers)
}

fn parse_id(value: &[u8]) -> Result<ObjectId> {
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidObject("tag target is not ASCII".into()))?;
    ObjectId::from_str(value).map_err(|_| Error::InvalidObject("invalid tag target".into()))
}

fn parse_kind(value: &[u8]) -> Result<ObjectKind> {
    match value {
        b"blob" => Ok(ObjectKind::Blob),
        b"tree" => Ok(ObjectKind::Tree),
        b"commit" => Ok(ObjectKind::Commit),
        b"tag" => Ok(ObjectKind::Tag),
        _ => Err(Error::InvalidObject("invalid annotated tag type".into())),
    }
}

const fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

#[cfg(test)]
mod tests {
    use super::{AnnotatedTag, MkTagOptions, TagBuilder};
    use crate::{
        CommitBuilder, ExtraHeader, InitOptions, MemoryFileSystem, ObjectId, ObjectKind,
        Repository, Signature, Tree,
    };

    #[test]
    fn annotated_tags_round_trip_headers_messages_and_signatures() {
        let repository = repository();
        let target = commit(&repository);
        let tagger = Signature::new("Tagger", "tagger@example.com", 1_700_000_000, 90).unwrap();
        let tag = TagBuilder::new(target, ObjectKind::Commit, b"v1.0", tagger.clone())
            .unwrap()
            .extra_header(ExtraHeader::new("encoding", b"UTF-8".to_vec()).unwrap())
            .message(b"release\n-----BEGIN PGP SIGNATURE-----\nbytes\n".to_vec())
            .build();
        let id = repository.write_tag(&tag, 4096).unwrap();
        let parsed = repository.read_tag(id, 4096).unwrap();
        assert_eq!(parsed, tag);
        assert_eq!(parsed.target(), target);
        assert_eq!(parsed.tagger(), Some(&tagger));
        assert!(parsed.message().ends_with(b"bytes\n"));
    }

    #[test]
    fn creates_lists_forces_and_deletes_lightweight_and_annotated_refs() {
        let repository = repository();
        let target = commit(&repository);
        repository
            .create_lightweight_tag("light", target, false, 4096)
            .unwrap();
        let tag = TagBuilder::new(
            target,
            ObjectKind::Commit,
            "annotated",
            Signature::new("Tagger", "tagger@example.com", 2, 0).unwrap(),
        )
        .unwrap()
        .message(b"message\n".to_vec())
        .build();
        let (_, tag_id) = repository
            .create_annotated_tag("annotated", &tag, false, 4096)
            .unwrap();
        assert_eq!(
            repository
                .tags()
                .unwrap()
                .iter()
                .map(crate::Reference::name)
                .collect::<Vec<_>>(),
            ["refs/tags/annotated", "refs/tags/light"]
        );
        assert!(
            repository
                .create_lightweight_tag("light", tag_id, false, 4096)
                .is_err()
        );
        repository
            .create_lightweight_tag("light", tag_id, true, 4096)
            .unwrap();
        repository.delete_tag("light", tag_id).unwrap();
        assert_eq!(repository.tags().unwrap().len(), 1);
    }

    #[test]
    fn peels_nested_tags_and_enforces_declared_types_and_depth() {
        let repository = repository();
        let target = commit(&repository);
        let signature = Signature::new("Tagger", "tagger@example.com", 3, 0).unwrap();
        let inner = TagBuilder::new(target, ObjectKind::Commit, "inner", signature.clone())
            .unwrap()
            .build();
        let inner_id = repository.write_tag(&inner, 4096).unwrap();
        let outer = TagBuilder::new(inner_id, ObjectKind::Tag, "outer", signature)
            .unwrap()
            .build();
        let outer_id = repository.write_tag(&outer, 4096).unwrap();
        let peeled = repository.peel_tag(outer_id, 2, 4096).unwrap();
        assert_eq!(peeled.id, target);
        assert_eq!(peeled.kind, ObjectKind::Commit);
        assert_eq!(peeled.depth, 2);
        assert!(repository.peel_tag(outer_id, 1, 4096).is_err());

        let wrong = TagBuilder::new(
            target,
            ObjectKind::Blob,
            "wrong",
            inner.tagger().unwrap().clone(),
        )
        .unwrap()
        .build();
        assert!(repository.write_tag(&wrong, 4096).is_err());
        assert!(AnnotatedTag::parse(b"object bad\ntype commit\ntag v\n\nmsg").is_err());
    }

    #[test]
    fn mktag_preserves_exact_bytes_and_matches_native_object_id() {
        let repository = repository();
        let target = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        let data = format!("object {target}\ntype blob\ntag v1\ntagger A <a@b> 1 +0000\n");
        let id = repository
            .mk_tag(data.as_bytes(), &MkTagOptions::default())
            .unwrap();
        assert_eq!(id.to_string(), "020af0de2a21636e20561ce438c29b6f06bcf00b");
        assert_eq!(
            repository.read_object(id, 4096).unwrap().data(),
            data.as_bytes()
        );
        assert_eq!(repository.read_tag(id, 4096).unwrap().message(), b"");
    }

    #[test]
    fn mktag_strictness_target_validation_dry_run_and_limits() {
        let repository = repository();
        let target = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        let warning = format!("object {target}\ntype blob\ntag bad..name\n\n");
        assert!(
            repository
                .mk_tag(warning.as_bytes(), &MkTagOptions::default())
                .is_err()
        );
        let id = repository
            .mk_tag(
                warning.as_bytes(),
                &MkTagOptions {
                    strict: false,
                    dry_run: true,
                    ..MkTagOptions::default()
                },
            )
            .unwrap();
        assert!(!repository.contains_object(id).unwrap());

        let wrong = format!("object {target}\ntype commit\ntag v1\ntagger A <a@b> 1 +0000\n");
        assert!(
            repository
                .mk_tag(wrong.as_bytes(), &MkTagOptions::default())
                .is_err()
        );
        assert!(
            repository
                .mk_tag(
                    warning.as_bytes(),
                    &MkTagOptions {
                        strict: false,
                        max_input_size: warning.len() - 1,
                        ..MkTagOptions::default()
                    },
                )
                .is_err()
        );
        let missing = ObjectId::from_bytes([0x11; ObjectId::LENGTH]);
        let missing_data = format!("object {missing}\ntype blob\ntag v1\ntagger A <a@b> 1 +0000\n");
        assert!(
            repository
                .mk_tag(missing_data.as_bytes(), &MkTagOptions::default())
                .is_err()
        );
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }

    fn commit(repository: &Repository) -> crate::ObjectId {
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("Author", "author@example.com", 1, 0).unwrap();
        repository
            .write_commit(
                &CommitBuilder::new(tree, identity.clone(), identity)
                    .message(b"commit\n".to_vec())
                    .build(),
            )
            .unwrap()
    }
}
