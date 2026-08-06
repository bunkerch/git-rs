//! Structured, bounded reference filtering and sorting.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::{
    Error, GraphOptions, ObjectId, ObjectKind, ReferenceTarget, Repository, Result, Signature,
};

/// A field used to order reference inventory entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefSortField {
    RefName,
    ObjectName,
    ObjectType,
    CreatorDate,
    AuthorDate,
    CommitterDate,
    TaggerDate,
}

/// One ordering key. Later keys are primary, matching Git's repeated `--sort`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefSortKey {
    pub field: RefSortField,
    pub descending: bool,
}

impl RefSortKey {
    #[must_use]
    pub const fn ascending(field: RefSortField) -> Self {
        Self {
            field,
            descending: false,
        }
    }

    #[must_use]
    pub const fn descending(field: RefSortField) -> Self {
        Self {
            field,
            descending: true,
        }
    }
}

/// Selection, ordering, and resource limits for [`Repository::for_each_ref`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForEachRefOptions {
    pub patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub points_at: Vec<ObjectId>,
    pub contains: Vec<ObjectId>,
    pub no_contains: Vec<ObjectId>,
    pub merged_into: Vec<ObjectId>,
    pub not_merged_into: Vec<ObjectId>,
    pub include_head: bool,
    pub ignore_case: bool,
    pub sort: Vec<RefSortKey>,
    pub start_after: Option<String>,
    pub count: Option<usize>,
    pub max_references: usize,
    pub max_depth: usize,
    pub max_tag_depth: usize,
    pub graph: GraphOptions,
}

impl Default for ForEachRefOptions {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            points_at: Vec::new(),
            contains: Vec::new(),
            no_contains: Vec::new(),
            merged_into: Vec::new(),
            not_merged_into: Vec::new(),
            include_head: false,
            ignore_case: false,
            sort: Vec::new(),
            start_after: None,
            count: None,
            max_references: 1_000_000,
            max_depth: 4096,
            max_tag_depth: 64,
            graph: GraphOptions::default(),
        }
    }
}

/// Object and identity metadata attached to a selected reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForEachRefEntry {
    name: String,
    object_id: ObjectId,
    object_kind: ObjectKind,
    symbolic_target: Option<String>,
    peeled_id: ObjectId,
    peeled_kind: ObjectKind,
    author: Option<Signature>,
    committer: Option<Signature>,
    tagger: Option<Signature>,
}

impl ForEachRefEntry {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }
    #[must_use]
    pub const fn object_kind(&self) -> ObjectKind {
        self.object_kind
    }
    #[must_use]
    pub fn symbolic_target(&self) -> Option<&str> {
        self.symbolic_target.as_deref()
    }
    #[must_use]
    pub const fn peeled_id(&self) -> ObjectId {
        self.peeled_id
    }
    #[must_use]
    pub const fn peeled_kind(&self) -> ObjectKind {
        self.peeled_kind
    }
    #[must_use]
    pub const fn author(&self) -> Option<&Signature> {
        self.author.as_ref()
    }
    #[must_use]
    pub const fn committer(&self) -> Option<&Signature> {
        self.committer.as_ref()
    }
    #[must_use]
    pub const fn tagger(&self) -> Option<&Signature> {
        self.tagger.as_ref()
    }

    fn creator_date(&self) -> Option<i64> {
        self.tagger
            .as_ref()
            .or(self.committer.as_ref())
            .map(Signature::timestamp)
    }
}

impl Repository {
    /// Select resolved references with Git-compatible matching and graph filters.
    ///
    /// Pattern literals match a complete name or a path prefix ending at `/`;
    /// wildcard patterns support `*`, `?`, bracket classes, and `**`. Annotated
    /// tags are peeled for metadata, points-at, and commit reachability filters.
    ///
    /// # Errors
    /// Returns an error for incompatible pagination options, malformed refs or
    /// objects, broken symbolic refs, or an exceeded object/graph/ref limit.
    #[allow(clippy::too_many_lines)]
    pub fn for_each_ref(&self, options: &ForEachRefOptions) -> Result<Vec<ForEachRefEntry>> {
        validate_options(options)?;
        let mut references = self.references_with_prefix_bounded(
            "refs/",
            options.max_references,
            options.max_depth,
        )?;
        if options.include_head {
            if references.len() == options.max_references {
                return Err(Error::InvalidRepository(
                    "reference enumeration exceeds limit".into(),
                ));
            }
            references.push(self.read_reference("HEAD")?);
        }

        let points_at = options.points_at.iter().copied().collect::<HashSet<_>>();
        let graph_filter = !options.contains.is_empty()
            || !options.no_contains.is_empty()
            || !options.merged_into.is_empty()
            || !options.not_merged_into.is_empty();
        let mut entries = Vec::new();
        for reference in references {
            let name = reference.name();
            if !matches_any(name, &options.patterns, options.ignore_case)
                || matches_any_nonempty(name, &options.exclude_patterns, options.ignore_case)
            {
                continue;
            }
            let symbolic_target = match reference.target() {
                ReferenceTarget::Direct(_) => None,
                ReferenceTarget::Symbolic(name) => Some(name.to_string()),
            };
            let object_id = match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(name)?,
            };
            let object = self.read_object(object_id, options.graph.max_object_size)?;
            let object_kind = object.kind();
            let mut current = object_id;
            let mut chain = vec![object_id];
            let mut tagger = None;
            for depth in 0..=options.max_tag_depth {
                let current_object = self.read_object(current, options.graph.max_object_size)?;
                if current_object.kind() != ObjectKind::Tag {
                    break;
                }
                if depth == options.max_tag_depth {
                    return Err(Error::InvalidObject(format!(
                        "annotated tag depth exceeds {}",
                        options.max_tag_depth
                    )));
                }
                let tag = self.read_tag(current, options.graph.max_object_size)?;
                if tagger.is_none() {
                    tagger = tag.tagger().cloned();
                }
                current = tag.target();
                let target = self.read_object(current, options.graph.max_object_size)?;
                if target.kind() != tag.target_kind() {
                    return Err(Error::InvalidObject("tag target type mismatch".into()));
                }
                chain.push(current);
            }
            if !points_at.is_empty() && !chain.iter().any(|id| points_at.contains(id)) {
                continue;
            }
            let peeled = self.read_object(current, options.graph.max_object_size)?;
            let peeled_kind = peeled.kind();
            if graph_filter && peeled_kind != ObjectKind::Commit {
                continue;
            }
            if !options.contains.is_empty()
                && !any_ancestor(self, &options.contains, current, false, &options.graph)?
            {
                continue;
            }
            if any_ancestor(self, &options.no_contains, current, false, &options.graph)? {
                continue;
            }
            if !options.merged_into.is_empty()
                && !any_ancestor(self, &options.merged_into, current, true, &options.graph)?
            {
                continue;
            }
            if any_ancestor(
                self,
                &options.not_merged_into,
                current,
                true,
                &options.graph,
            )? {
                continue;
            }
            let (author, committer) = if peeled_kind == ObjectKind::Commit {
                let commit = self.read_commit(current, options.graph.max_object_size)?;
                (
                    Some(commit.author().clone()),
                    Some(commit.committer().clone()),
                )
            } else {
                (None, None)
            };
            entries.push(ForEachRefEntry {
                name: name.to_owned(),
                object_id,
                object_kind,
                symbolic_target,
                peeled_id: current,
                peeled_kind,
                author,
                committer,
                tagger,
            });
        }

        let keys = if options.sort.is_empty() {
            vec![RefSortKey::ascending(RefSortField::RefName)]
        } else {
            options.sort.clone()
        };
        entries.sort_by(|left, right| compare_entries(left, right, &keys, options.ignore_case));
        if let Some(marker) = &options.start_after {
            entries.retain(|entry| entry.name.as_bytes() > marker.as_bytes());
        }
        entries.truncate(options.count.unwrap_or(usize::MAX));
        Ok(entries)
    }
}

fn any_ancestor(
    repository: &Repository,
    ids: &[ObjectId],
    candidate: ObjectId,
    candidate_is_ancestor: bool,
    graph: &GraphOptions,
) -> Result<bool> {
    for id in ids {
        let (ancestor, descendant) = if candidate_is_ancestor {
            (candidate, *id)
        } else {
            (*id, candidate)
        };
        if repository.is_ancestor(ancestor, descendant, graph)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_options(options: &ForEachRefOptions) -> Result<()> {
    if options
        .patterns
        .iter()
        .chain(&options.exclude_patterns)
        .any(String::is_empty)
    {
        return Err(Error::InvalidReference("empty for-each-ref pattern".into()));
    }
    if options.start_after.is_some() && (!options.patterns.is_empty() || !options.sort.is_empty()) {
        return Err(Error::InvalidReference(
            "start-after cannot be combined with patterns or explicit sorting".into(),
        ));
    }
    Ok(())
}

fn compare_entries(
    a: &ForEachRefEntry,
    b: &ForEachRefEntry,
    keys: &[RefSortKey],
    fold: bool,
) -> Ordering {
    for key in keys.iter().rev() {
        let order = match key.field {
            RefSortField::RefName => compare_text(&a.name, &b.name, fold),
            RefSortField::ObjectName => a.object_id.cmp(&b.object_id),
            RefSortField::ObjectType => kind_name(a.object_kind).cmp(kind_name(b.object_kind)),
            RefSortField::CreatorDate => a
                .creator_date()
                .unwrap_or(0)
                .cmp(&b.creator_date().unwrap_or(0)),
            RefSortField::AuthorDate => {
                timestamp(a.author.as_ref()).cmp(&timestamp(b.author.as_ref()))
            }
            RefSortField::CommitterDate => {
                timestamp(a.committer.as_ref()).cmp(&timestamp(b.committer.as_ref()))
            }
            RefSortField::TaggerDate => {
                timestamp(a.tagger.as_ref()).cmp(&timestamp(b.tagger.as_ref()))
            }
        };
        let order = if key.descending {
            order.reverse()
        } else {
            order
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.name.as_bytes().cmp(b.name.as_bytes())
}

const fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
        ObjectKind::Tree => "tree",
    }
}

fn timestamp(signature: Option<&Signature>) -> i64 {
    signature.map_or(0, Signature::timestamp)
}

fn compare_text(a: &str, b: &str, fold: bool) -> Ordering {
    if fold {
        a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
    } else {
        a.cmp(b)
    }
}

fn matches_any(name: &str, patterns: &[String], fold: bool) -> bool {
    patterns.is_empty() || matches_any_nonempty(name, patterns, fold)
}

fn matches_any_nonempty(name: &str, patterns: &[String], fold: bool) -> bool {
    patterns
        .iter()
        .any(|pattern| path_pattern(pattern, name, fold))
}

fn path_pattern(pattern: &str, name: &str, fold: bool) -> bool {
    let (pattern, name) = if fold {
        (pattern.to_ascii_lowercase(), name.to_ascii_lowercase())
    } else {
        (pattern.to_owned(), name.to_owned())
    };
    if !pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        return name == pattern
            || (name.starts_with(&pattern)
                && (pattern.ends_with('/') || name.as_bytes().get(pattern.len()) == Some(&b'/')));
    }
    wildcard(pattern.as_bytes(), name.as_bytes())
}

fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    fn inner(p: &[u8], t: &[u8], seen: &mut HashSet<(usize, usize)>, po: usize, to: usize) -> bool {
        if !seen.insert((po, to)) {
            return false;
        }
        if po == p.len() {
            return to == t.len();
        }
        match p[po] {
            b'*' => {
                let double = p.get(po + 1) == Some(&b'*');
                let next = po + if double { 2 } else { 1 };
                inner(p, t, seen, next, to)
                    || (to < t.len() && (double || t[to] != b'/') && inner(p, t, seen, po, to + 1))
            }
            b'?' => to < t.len() && t[to] != b'/' && inner(p, t, seen, po + 1, to + 1),
            b'[' => {
                let Some(end) = p[po + 1..]
                    .iter()
                    .position(|byte| *byte == b']')
                    .map(|n| po + 1 + n)
                else {
                    return to < t.len() && t[to] == b'[' && inner(p, t, seen, po + 1, to + 1);
                };
                to < t.len()
                    && t[to] != b'/'
                    && class_matches(&p[po + 1..end], t[to])
                    && inner(p, t, seen, end + 1, to + 1)
            }
            byte => to < t.len() && byte == t[to] && inner(p, t, seen, po + 1, to + 1),
        }
    }
    inner(pattern, text, &mut HashSet::new(), 0, 0)
}

fn class_matches(class: &[u8], byte: u8) -> bool {
    let (negated, class) = if class.first().is_some_and(|b| matches!(b, b'!' | b'^')) {
        (true, &class[1..])
    } else {
        (false, class)
    };
    let mut matched = false;
    let mut index = 0;
    while index < class.len() {
        if index + 2 < class.len() && class[index + 1] == b'-' {
            matched |= (class[index]..=class[index + 2]).contains(&byte);
            index += 3;
        } else {
            matched |= class[index] == byte;
            index += 1;
        }
    }
    matched != negated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, PreviousValue, ReferenceName, TagBuilder,
        Tree,
    };

    #[test]
    fn path_patterns_respect_components_and_wildcards() {
        assert!(path_pattern("refs/heads", "refs/heads/main", false));
        assert!(!path_pattern("refs/head", "refs/heads/main", false));
        assert!(path_pattern("refs/heads/m*", "refs/heads/main", false));
        assert!(!path_pattern("refs/*", "refs/heads/main", false));
        assert!(path_pattern("refs/**/m[ae]in", "refs/heads/main", false));
        assert!(path_pattern("REFS/HEADS/MAIN", "refs/heads/main", true));
    }

    #[test]
    fn filters_graphs_nested_tags_and_sorts_metadata() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = |name, time| Signature::new(name, "a@example.com", time, 0).unwrap();
        let base = repository
            .write_commit(
                &CommitBuilder::new(tree, identity("Base", 1), identity("Base", 1))
                    .message(b"base\n".to_vec())
                    .build(),
            )
            .unwrap();
        let tip = repository
            .write_commit(
                &CommitBuilder::new(tree, identity("Tip", 2), identity("Tip", 2))
                    .parent(base)
                    .message(b"tip\n".to_vec())
                    .build(),
            )
            .unwrap();
        for (name, id) in [("main", tip), ("old", base)] {
            repository
                .update_reference(
                    &ReferenceName::branch(name).unwrap(),
                    id,
                    PreviousValue::Any,
                )
                .unwrap();
        }
        let inner = repository
            .create_annotated_tag(
                "inner",
                &TagBuilder::new(tip, ObjectKind::Commit, b"inner", identity("Inner", 3))
                    .unwrap()
                    .build(),
                false,
                4096,
            )
            .unwrap();
        let inner_id = inner.1;
        repository
            .create_annotated_tag(
                "outer",
                &TagBuilder::new(inner_id, ObjectKind::Tag, b"outer", identity("Outer", 4))
                    .unwrap()
                    .build(),
                false,
                4096,
            )
            .unwrap();

        let entries = repository
            .for_each_ref(&ForEachRefOptions {
                patterns: vec!["refs/heads".into(), "refs/tags/outer".into()],
                contains: vec![base],
                points_at: vec![inner_id],
                sort: vec![RefSortKey::descending(RefSortField::CreatorDate)],
                ..ForEachRefOptions::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "refs/tags/outer");
        assert_eq!(entries[0].object_kind(), ObjectKind::Tag);
        assert_eq!(entries[0].peeled_id(), tip);
        assert_eq!(entries[0].tagger().unwrap().timestamp(), 4);

        let merged = repository
            .for_each_ref(&ForEachRefOptions {
                patterns: vec!["refs/heads".into()],
                merged_into: vec![tip],
                sort: vec![RefSortKey::descending(RefSortField::CommitterDate)],
                ..ForEachRefOptions::default()
            })
            .unwrap();
        assert_eq!(
            merged.iter().map(ForEachRefEntry::name).collect::<Vec<_>>(),
            ["refs/heads/main", "refs/heads/old"]
        );
    }

    #[test]
    fn pagination_and_later_primary_sort_keys_match_git_rules() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        for name in ["a", "b", "c"] {
            repository
                .update_reference(
                    &ReferenceName::branch(name).unwrap(),
                    blob,
                    PreviousValue::Any,
                )
                .unwrap();
        }
        let entries = repository
            .for_each_ref(&ForEachRefOptions {
                start_after: Some("refs/heads/a".into()),
                count: Some(1),
                ..ForEachRefOptions::default()
            })
            .unwrap();
        assert_eq!(entries[0].name(), "refs/heads/b");
        assert!(
            repository
                .for_each_ref(&ForEachRefOptions {
                    patterns: vec!["refs/heads".into()],
                    start_after: Some("refs/heads/a".into()),
                    ..ForEachRefOptions::default()
                })
                .is_err()
        );
    }
}
