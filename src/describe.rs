//! Bounded nearest-reference descriptions for commits.

use std::collections::{BTreeMap, HashSet};

use crate::ignore::wildmatch;
use crate::{
    Error, GraphOptions, ObjectId, ObjectKind, ReferenceTarget, Repository, Result,
    RevisionOptions, RevisionWalkOptions,
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescribeOptions {
    pub all: bool,
    pub tags: bool,
    pub long: bool,
    pub first_parent: bool,
    pub always: bool,
    pub exact_match: bool,
    pub max_candidates: usize,
    pub abbreviation: usize,
    pub match_patterns: Vec<Vec<u8>>,
    pub exclude_patterns: Vec<Vec<u8>>,
    pub graph: GraphOptions,
    pub max_references: usize,
    pub max_reference_depth: usize,
    pub max_tag_depth: usize,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        Self {
            all: false,
            tags: false,
            long: false,
            first_parent: false,
            always: false,
            exact_match: false,
            max_candidates: 10,
            abbreviation: 7,
            match_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            graph: GraphOptions::default(),
            max_references: 1_000_000,
            max_reference_depth: 4096,
            max_tag_depth: 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Description {
    rendered: String,
    name: Option<String>,
    target: ObjectId,
    depth: usize,
    abbreviation: String,
    exact: bool,
    annotated: bool,
}

impl Description {
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[must_use]
    pub const fn target(&self) -> ObjectId {
        self.target
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    #[must_use]
    pub fn abbreviation(&self) -> &str {
        &self.abbreviation
    }

    #[must_use]
    pub const fn is_exact(&self) -> bool {
        self.exact
    }

    #[must_use]
    pub const fn is_annotated(&self) -> bool {
        self.annotated
    }
}

#[derive(Clone)]
struct CommitName {
    display: String,
    priority: u8,
    tagger_time: i64,
    annotated: bool,
    misnamed: bool,
}

impl Repository {
    /// Describe a commit using the nearest eligible reachable reference.
    ///
    /// Annotated tags are the default. `tags` admits lightweight tags and
    /// `all` admits all references. Candidate depth is the number of commits
    /// reachable from the target but not from the named commit.
    ///
    /// # Errors
    /// Returns an error for invalid options/patterns/revisions, missing or
    /// malformed refs/tags/commits, no eligible name without `always`, exceeded
    /// graph/reference/object bounds, or storage failures.
    pub fn describe(&self, target: &str, options: &DescribeOptions) -> Result<Description> {
        validate_options(options)?;
        let resolved = self.resolve_revision(
            target,
            &RevisionOptions {
                max_object_size: options.graph.max_object_size,
                ..RevisionOptions::default()
            },
        )?;
        let target_id = match resolved.kind {
            ObjectKind::Commit => resolved.id,
            ObjectKind::Tag => {
                let peeled = self.peel_tag(
                    resolved.id,
                    options.max_tag_depth,
                    options.graph.max_object_size,
                )?;
                if peeled.kind != ObjectKind::Commit {
                    return Err(Error::InvalidRevision(
                        "describe target does not peel to a commit".into(),
                    ));
                }
                peeled.id
            }
            ObjectKind::Blob | ObjectKind::Tree => {
                return Err(Error::InvalidRevision(
                    "describe target is not a commit-ish".into(),
                ));
            }
        };
        self.read_commit(target_id, options.graph.max_object_size)?;
        let names = self.describe_names(options)?;
        if let Some(name) = names.get(&target_id)
            && eligible(name, options)
        {
            return self.finish_description(target_id, name, 0, true, options);
        }
        if options.exact_match || options.max_candidates == 0 {
            return Err(Error::InvalidRevision(format!(
                "no name exactly matches {target_id}"
            )));
        }

        let walk_options = RevisionWalkOptions {
            graph: options.graph.clone(),
            first_parent: options.first_parent,
            ..RevisionWalkOptions::default()
        };
        let target_walk = self.walk_revisions(&[target_id], &[], &walk_options)?;
        let target_set = target_walk
            .iter()
            .map(crate::Revision::id)
            .collect::<HashSet<_>>();
        let mut found = Vec::new();
        for revision in &target_walk {
            if let Some(name) = names.get(&revision.id())
                && eligible(name, options)
            {
                found.push((revision.id(), name.clone(), found.len()));
                if found.len() == options.max_candidates {
                    break;
                }
            }
        }
        if found.is_empty() {
            if options.always {
                let abbreviation = self.unique_abbreviation(
                    target_id,
                    options.abbreviation.max(1),
                    options.graph.max_commits,
                )?;
                return Ok(Description {
                    rendered: abbreviation.clone(),
                    name: None,
                    target: target_id,
                    depth: 0,
                    abbreviation,
                    exact: false,
                    annotated: false,
                });
            }
            return Err(Error::InvalidRevision(format!(
                "no eligible name can describe {target_id}"
            )));
        }
        let mut ranked = Vec::with_capacity(found.len());
        for (id, name, order) in found {
            let candidate_walk = self.walk_revisions(&[id], &[], &walk_options)?;
            let named_set = candidate_walk
                .iter()
                .map(crate::Revision::id)
                .collect::<HashSet<_>>();
            let depth = target_set.difference(&named_set).count();
            ranked.push((depth, order, id, name));
        }
        ranked.sort_unstable_by_key(|(depth, order, _, _)| (*depth, *order));
        let (depth, _, _, name) = ranked
            .into_iter()
            .next()
            .ok_or_else(|| Error::InvalidRevision("describe has no candidate".into()))?;
        self.finish_description(target_id, &name, depth, false, options)
    }

    fn describe_names(&self, options: &DescribeOptions) -> Result<BTreeMap<ObjectId, CommitName>> {
        let mut output = BTreeMap::new();
        for reference in self.references_with_prefix_bounded(
            "refs/",
            options.max_references,
            options.max_reference_depth,
        )? {
            let full_name = reference.name();
            let (display_path, pattern_path, is_tag) =
                if let Some(short) = full_name.strip_prefix("refs/tags/") {
                    (short, short, true)
                } else if options.all {
                    let Some(display) = full_name.strip_prefix("refs/") else {
                        continue;
                    };
                    let patterns_active =
                        !options.match_patterns.is_empty() || !options.exclude_patterns.is_empty();
                    let pattern = if let Some(short) = full_name.strip_prefix("refs/heads/") {
                        short
                    } else if let Some(short) = full_name.strip_prefix("refs/remotes/") {
                        short
                    } else if patterns_active {
                        continue;
                    } else {
                        display
                    };
                    (display, pattern, false)
                } else {
                    continue;
                };
            if !pattern_selected(pattern_path.as_bytes(), options) {
                continue;
            }
            let id = match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(full_name)?,
            };
            let object = self.read_object(id, options.graph.max_object_size)?;
            let (peeled, annotated, display, tagger_time, misnamed) =
                if object.kind() == ObjectKind::Tag {
                    let outer = self.read_tag(id, options.graph.max_object_size)?;
                    let peeled =
                        self.peel_tag(id, options.max_tag_depth, options.graph.max_object_size)?;
                    if peeled.kind != ObjectKind::Commit {
                        continue;
                    }
                    let header = std::str::from_utf8(outer.name())
                        .map_err(|_| Error::InvalidObject("non-UTF-8 annotated tag name".into()))?;
                    (
                        peeled.id,
                        true,
                        if options.all {
                            format!("tags/{header}")
                        } else {
                            header.to_owned()
                        },
                        outer.tagger().map_or(0, crate::Signature::timestamp),
                        header != display_path,
                    )
                } else {
                    if object.kind() != ObjectKind::Commit {
                        continue;
                    }
                    (
                        id,
                        false,
                        if options.all && is_tag {
                            format!("tags/{display_path}")
                        } else {
                            display_path.to_owned()
                        },
                        0,
                        false,
                    )
                };
            let priority = if annotated { 2 } else { u8::from(is_tag) };
            let candidate = CommitName {
                display,
                priority,
                tagger_time,
                annotated,
                misnamed,
            };
            let replace = output.get(&peeled).is_none_or(|current: &CommitName| {
                candidate.priority > current.priority
                    || (candidate.priority == 2
                        && current.priority == 2
                        && candidate.tagger_time > current.tagger_time)
            });
            if replace {
                output.insert(peeled, candidate);
            }
        }
        Ok(output)
    }

    fn finish_description(
        &self,
        target: ObjectId,
        name: &CommitName,
        depth: usize,
        exact: bool,
        options: &DescribeOptions,
    ) -> Result<Description> {
        let abbreviation = self.unique_abbreviation(
            target,
            options.abbreviation.max(1),
            options.graph.max_commits,
        )?;
        let suffix = options.long || name.misnamed || (!exact && options.abbreviation != 0);
        let rendered = if suffix {
            format!("{}-{depth}-g{abbreviation}", name.display)
        } else {
            name.display.clone()
        };
        Ok(Description {
            rendered,
            name: Some(name.display.clone()),
            target,
            depth,
            abbreviation,
            exact,
            annotated: name.annotated,
        })
    }
}

fn eligible(name: &CommitName, options: &DescribeOptions) -> bool {
    options.all || options.tags || name.annotated
}

fn pattern_selected(name: &[u8], options: &DescribeOptions) -> bool {
    !options
        .exclude_patterns
        .iter()
        .any(|pattern| wildmatch(pattern, name))
        && (options.match_patterns.is_empty()
            || options
                .match_patterns
                .iter()
                .any(|pattern| wildmatch(pattern, name)))
}

fn validate_options(options: &DescribeOptions) -> Result<()> {
    if options.abbreviation >= ObjectId::HEX_LENGTH {
        return Err(Error::InvalidRevision(
            "describe abbreviation must be between 0 and 39".into(),
        ));
    }
    if options.long && options.abbreviation == 0 {
        return Err(Error::InvalidRevision(
            "long describe format requires an abbreviation".into(),
        ));
    }
    if options
        .match_patterns
        .iter()
        .chain(&options.exclude_patterns)
        .any(|pattern| pattern.contains(&0))
    {
        return Err(Error::InvalidRevision(
            "describe pattern contains NUL".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, PreviousValue, ReferenceName, Signature,
        TagBuilder, Tree,
    };

    #[test]
    fn prefers_annotated_tags_and_computes_merge_distance() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], 1);
        let left = commit(&repository, &[root], 2);
        let right = commit(&repository, &[root], 3);
        let merge = commit(&repository, &[left, right], 4);
        repository
            .create_lightweight_tag("light", left, false, 4096)
            .unwrap();
        let tag = TagBuilder::new(
            root,
            ObjectKind::Commit,
            "v1",
            Signature::new("Tagger", "tagger@example.com", 5, 0).unwrap(),
        )
        .unwrap()
        .build();
        repository
            .create_annotated_tag("v1", &tag, false, 4096)
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                merge,
                PreviousValue::Any,
            )
            .unwrap();

        let described = repository
            .describe("HEAD", &DescribeOptions::default())
            .unwrap();
        assert_eq!(described.name(), Some("v1"));
        assert_eq!(described.depth(), 3);
        assert!(described.rendered().starts_with("v1-3-g"));
        let tags = repository
            .describe(
                &left.to_string(),
                &DescribeOptions {
                    tags: true,
                    ..DescribeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(tags.rendered(), "light");
        assert!(tags.is_exact());
    }

    #[test]
    fn supports_all_patterns_first_parent_exact_and_always() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], 1);
        let tip = commit(&repository, &[root], 2);
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                root,
                PreviousValue::Any,
            )
            .unwrap();
        let all = repository
            .describe(
                &tip.to_string(),
                &DescribeOptions {
                    all: true,
                    match_patterns: vec![b"main".to_vec()],
                    ..DescribeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(all.name(), Some("heads/main"));
        assert_eq!(all.depth(), 1);
        assert!(
            repository
                .describe(
                    &tip.to_string(),
                    &DescribeOptions {
                        exact_match: true,
                        all: true,
                        ..DescribeOptions::default()
                    },
                )
                .is_err()
        );
        let fallback =
            Repository::init(MemoryFileSystem::new(), "fallback", &InitOptions::default()).unwrap();
        let commit = commit(&fallback, &[], 1);
        assert!(
            fallback
                .describe(
                    &commit.to_string(),
                    &DescribeOptions {
                        always: true,
                        ..DescribeOptions::default()
                    },
                )
                .unwrap()
                .name()
                .is_none()
        );
    }

    fn commit(repository: &Repository, parents: &[ObjectId], timestamp: i64) -> ObjectId {
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("Test", "test@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
