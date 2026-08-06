//! Ordered, resolved reference inventory.

use crate::{Error, ObjectId, ObjectKind, ReferenceTarget, Repository, Result};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowRefOptions {
    pub include_head: bool,
    pub branches_only: bool,
    pub tags_only: bool,
    pub dereference_tags: bool,
    pub patterns: Vec<String>,
    pub max_references: usize,
    pub max_depth: usize,
    pub max_tag_depth: usize,
    pub max_object_size: usize,
}

impl Default for ShowRefOptions {
    fn default() -> Self {
        Self {
            include_head: false,
            branches_only: false,
            tags_only: false,
            dereference_tags: false,
            patterns: Vec::new(),
            max_references: 1_000_000,
            max_depth: 4096,
            max_tag_depth: 64,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowRefEntry {
    name: String,
    id: ObjectId,
    peeled: bool,
}

impl ShowRefEntry {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub const fn is_peeled(&self) -> bool {
        self.peeled
    }
}

impl Repository {
    /// List resolved references in Git's bytewise reference order.
    ///
    /// Loose references override packed references. Optional patterns match a
    /// complete reference name or a slash-delimited suffix, as in `show-ref`.
    /// `HEAD`, when requested, precedes the `refs/` namespace.
    ///
    /// # Errors
    /// Returns an error for malformed, broken, cyclic, or missing references or
    /// objects, malformed tags, exceeded resource limits, or storage failures.
    pub fn show_refs(&self, options: &ShowRefOptions) -> Result<Vec<ShowRefEntry>> {
        if options.patterns.iter().any(String::is_empty) {
            return Err(Error::InvalidReference("empty show-ref pattern".into()));
        }
        let references = self.references_with_prefix_bounded(
            "refs/",
            options.max_references,
            options.max_depth,
        )?;
        let mut output = Vec::new();
        if options.include_head {
            let id = self.resolve_reference("HEAD")?;
            self.validate_show_ref_object(id, options.max_object_size)?;
            output.push(ShowRefEntry {
                name: "HEAD".into(),
                id,
                peeled: false,
            });
        }
        for reference in references {
            let name = reference.name();
            if (options.branches_only || options.tags_only)
                && !((options.branches_only && name.starts_with("refs/heads/"))
                    || (options.tags_only && name.starts_with("refs/tags/")))
            {
                continue;
            }
            if !matches_patterns(name, &options.patterns) {
                continue;
            }
            let id = match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(name)?,
            };
            let kind = self.validate_show_ref_object(id, options.max_object_size)?;
            output.push(ShowRefEntry {
                name: name.to_owned(),
                id,
                peeled: false,
            });
            if options.dereference_tags && kind == ObjectKind::Tag {
                let peeled = self.peel_tag(id, options.max_tag_depth, options.max_object_size)?;
                output.push(ShowRefEntry {
                    name: format!("{name}^{{}}"),
                    id: peeled.id,
                    peeled: true,
                });
            }
            if output.len() > options.max_references.saturating_mul(2).saturating_add(1) {
                return Err(Error::InvalidRepository(
                    "show-ref exceeds output limit".into(),
                ));
            }
        }
        Ok(output)
    }

    fn validate_show_ref_object(&self, id: ObjectId, max_size: usize) -> Result<ObjectKind> {
        self.read_object(id, max_size).map(|object| object.kind())
    }
}

fn matches_patterns(name: &str, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            name == pattern
                || (name.ends_with(pattern)
                    && name.as_bytes().get(name.len() - pattern.len() - 1) == Some(&b'/'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        InitOptions, MemoryFileSystem, ObjectKind, PreviousValue, ReferenceName, Signature,
        TagBuilder,
    };

    #[test]
    fn lists_filters_resolves_and_peels_references() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"blob").unwrap();
        let tag = TagBuilder::new(
            blob,
            ObjectKind::Blob,
            b"v1",
            Signature::new("Tagger", "tagger@example.com", 1, 0).unwrap(),
        )
        .unwrap()
        .build();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                blob,
                PreviousValue::Any,
            )
            .unwrap();
        repository
            .create_annotated_tag("v1", &tag, false, 4096)
            .unwrap();

        let entries = repository
            .show_refs(&ShowRefOptions {
                include_head: true,
                dereference_tags: true,
                ..ShowRefOptions::default()
            })
            .unwrap();
        assert_eq!(entries[0].name(), "HEAD");
        assert_eq!(entries[0].id(), blob);
        assert!(entries.iter().any(|entry| entry.name() == "refs/tags/v1"));
        assert!(entries.iter().any(|entry| {
            entry.name() == "refs/tags/v1^{}" && entry.id() == blob && entry.is_peeled()
        }));
        let tags = repository
            .show_refs(&ShowRefOptions {
                tags_only: true,
                patterns: vec!["v1".into()],
                ..ShowRefOptions::default()
            })
            .unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name(), "refs/tags/v1");
        assert!(
            repository
                .show_refs(&ShowRefOptions {
                    max_references: 1,
                    ..ShowRefOptions::default()
                })
                .is_err()
        );
    }
}
