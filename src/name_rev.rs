//! Bounded human-readable names for objects relative to repository refs.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::ignore::wildmatch_ref;
use crate::{Error, GraphOptions, ObjectId, ObjectKind, ReferenceTarget, Repository, Result};

const MERGE_TRAVERSAL_WEIGHT: usize = 65_535;
const CUTOFF_SLOP_SECONDS: i64 = 86_400;

/// Reference selection, fallback, and resource limits for name-rev queries.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct NameRevOptions {
    pub tags_only: bool,
    pub include_patterns: Vec<Vec<u8>>,
    pub exclude_patterns: Vec<Vec<u8>>,
    /// Use tag basenames when tags are the only eligible namespace.
    pub shorten_tags: bool,
    /// Traverse without the target-date cutoff used by ordinary `name-rev`.
    pub all: bool,
    /// Return a unique object abbreviation when no ref-derived name exists.
    pub always: bool,
    /// Return `None` for unnamed objects. This takes precedence over `always`.
    pub allow_undefined: bool,
    pub abbreviation: usize,
    pub graph: GraphOptions,
    pub max_references: usize,
    pub max_reference_depth: usize,
    pub max_tag_depth: usize,
    pub max_traversal_updates: usize,
    pub max_abbreviation_candidates: usize,
}

impl Default for NameRevOptions {
    fn default() -> Self {
        Self {
            tags_only: false,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            shorten_tags: false,
            all: false,
            always: false,
            allow_undefined: true,
            abbreviation: 7,
            graph: GraphOptions::default(),
            max_references: 1_000_000,
            max_reference_depth: 4096,
            max_tag_depth: 64,
            max_traversal_updates: 50_000_000,
            max_abbreviation_candidates: 10_000_000,
        }
    }
}

/// A name for one requested object, preserving caller order and duplicates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameRevEntry {
    id: ObjectId,
    kind: ObjectKind,
    name: Option<String>,
    fallback: bool,
}

impl NameRevEntry {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    #[must_use]
    pub const fn is_fallback(&self) -> bool {
        self.fallback
    }
}

#[derive(Clone)]
struct Tip {
    object_id: ObjectId,
    commit_id: Option<ObjectId>,
    name: String,
    tagger_date: i64,
    from_tag: bool,
    dereferenced: bool,
}

#[derive(Clone)]
struct CommitName {
    tip_name: String,
    tagger_date: i64,
    generation: usize,
    distance: usize,
    from_tag: bool,
}

impl Repository {
    /// Name objects using the best reachable ref and Git's first/merge-parent notation.
    ///
    /// Tags beat non-tags; tag candidates prefer effective proximity, while
    /// non-tags use proximity and then older tip dates. First-parent traversal
    /// produces `~N`; other parents produce `^N` and retain subsequent `~N`.
    /// Non-commit objects can receive an exact direct-ref name.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt objects, malformed refs/tags/history,
    /// invalid options, or exceeded reference, graph, traversal, or abbreviation limits.
    pub fn name_revs(
        &self,
        targets: &[ObjectId],
        options: &NameRevOptions,
    ) -> Result<Vec<NameRevEntry>> {
        validate_options(options)?;
        let mut target_objects = Vec::with_capacity(targets.len());
        let mut cutoff = i64::MAX;
        for id in targets {
            let object = self.read_object(*id, options.graph.max_object_size)?;
            let kind = object.kind();
            if let Some(commit_id) = self.peel_name_rev_commit(*id, kind, options)? {
                let date = self
                    .read_commit(commit_id, options.graph.max_object_size)?
                    .committer()
                    .timestamp();
                cutoff = cutoff.min(date.saturating_sub(CUTOFF_SLOP_SECONDS));
            }
            target_objects.push((*id, kind));
        }
        if options.all {
            cutoff = i64::MIN;
        }

        let mut tips = self.name_rev_tips(options)?;
        tips.sort_by(compare_tips);
        let mut exact = HashMap::new();
        for tip in &tips {
            exact
                .entry(tip.object_id)
                .or_insert_with(|| tip.name.clone());
        }
        let mut names = HashMap::<ObjectId, CommitName>::new();
        let mut updates = 0usize;
        for tip in &tips {
            let Some(commit_id) = tip.commit_id else {
                continue;
            };
            self.spread_name_rev_tip(commit_id, tip, cutoff, options, &mut names, &mut updates)?;
        }

        let mut output = Vec::with_capacity(targets.len());
        for (id, kind) in target_objects {
            let mut name = if kind == ObjectKind::Commit {
                names.get(&id).map(render_name)
            } else {
                exact.get(&id).cloned()
            };
            let fallback = name.is_none() && !options.allow_undefined && options.always;
            if fallback {
                name = Some(self.unique_abbreviation(
                    id,
                    options.abbreviation,
                    options.max_abbreviation_candidates,
                )?);
            }
            if name.is_none() && !options.allow_undefined {
                return Err(Error::InvalidRepository(format!("cannot name object {id}")));
            }
            output.push(NameRevEntry {
                id,
                kind,
                name,
                fallback,
            });
        }
        Ok(output)
    }

    fn name_rev_tips(&self, options: &NameRevOptions) -> Result<Vec<Tip>> {
        let references = self.references_with_prefix_bounded(
            "refs/",
            options.max_references,
            options.max_reference_depth,
        )?;
        let mut tips = Vec::new();
        for reference in references {
            let full_name = reference.name();
            if options.tags_only && !full_name.starts_with("refs/tags/") {
                continue;
            }
            if options
                .exclude_patterns
                .iter()
                .any(|pattern| subpath_match(full_name.as_bytes(), pattern).is_some())
            {
                continue;
            }
            let mut shorten = options.tags_only && options.shorten_tags;
            if !options.include_patterns.is_empty() {
                let mut matched = false;
                for pattern in &options.include_patterns {
                    if let Some(offset) = subpath_match(full_name.as_bytes(), pattern) {
                        matched = true;
                        shorten |= offset > 0;
                    }
                }
                if !matched {
                    continue;
                }
            }
            let object_id = match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(full_name)?,
            };
            let object = self.read_object(object_id, options.graph.max_object_size)?;
            let from_tag = full_name.starts_with("refs/tags/");
            let name = shorten_name(full_name, shorten);
            let (commit_id, tagger_date, dereferenced) =
                self.peel_name_rev_tip(object_id, object.kind(), options)?;
            tips.push(Tip {
                object_id,
                commit_id,
                name,
                tagger_date,
                from_tag,
                dereferenced,
            });
        }
        Ok(tips)
    }

    fn peel_name_rev_tip(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        options: &NameRevOptions,
    ) -> Result<(Option<ObjectId>, i64, bool)> {
        let mut current = id;
        let mut current_kind = kind;
        let mut tagger_date = None;
        let mut seen = HashSet::new();
        for depth in 0..=options.max_tag_depth {
            if current_kind != ObjectKind::Tag {
                break;
            }
            if depth == options.max_tag_depth {
                return Err(Error::InvalidObject(format!(
                    "annotated tag depth exceeds {}",
                    options.max_tag_depth
                )));
            }
            if !seen.insert(current) {
                return Err(Error::InvalidObject("annotated tag cycle".into()));
            }
            let tag = self.read_tag(current, options.graph.max_object_size)?;
            tagger_date = tag.tagger().map(crate::Signature::timestamp);
            current = tag.target();
            let target = self.read_object(current, options.graph.max_object_size)?;
            if target.kind() != tag.target_kind() {
                return Err(Error::InvalidObject("tag target type mismatch".into()));
            }
            current_kind = target.kind();
        }
        if current_kind == ObjectKind::Commit {
            let commit_date = self
                .read_commit(current, options.graph.max_object_size)?
                .committer()
                .timestamp();
            Ok((
                Some(current),
                tagger_date.unwrap_or(commit_date),
                current != id,
            ))
        } else {
            Ok((None, tagger_date.unwrap_or(i64::MAX), current != id))
        }
    }

    fn peel_name_rev_commit(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        options: &NameRevOptions,
    ) -> Result<Option<ObjectId>> {
        self.peel_name_rev_tip(id, kind, options)
            .map(|value| value.0)
    }

    fn spread_name_rev_tip(
        &self,
        start: ObjectId,
        tip: &Tip,
        cutoff: i64,
        options: &NameRevOptions,
        names: &mut HashMap<ObjectId, CommitName>,
        updates: &mut usize,
    ) -> Result<()> {
        if self
            .read_commit(start, options.graph.max_object_size)?
            .committer()
            .timestamp()
            < cutoff
        {
            return Ok(());
        }
        let start_name = if tip.dereferenced {
            format!("{}^0", tip.name)
        } else {
            tip.name.clone()
        };
        if !update_name(
            names,
            start,
            CommitName {
                tip_name: start_name,
                tagger_date: tip.tagger_date,
                generation: 0,
                distance: 0,
                from_tag: tip.from_tag,
            },
        ) {
            return Ok(());
        }
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            *updates = updates.saturating_add(1);
            if *updates > options.max_traversal_updates {
                return Err(Error::InvalidRepository(
                    "name-rev traversal update limit exceeded".into(),
                ));
            }
            if names.len() > options.graph.max_commits {
                return Err(Error::InvalidRepository(
                    "name-rev commit limit exceeded".into(),
                ));
            }
            let commit = self.read_commit(id, options.graph.max_object_size)?;
            let current = names[&id].clone();
            let mut queued = Vec::new();
            for (index, parent) in commit.parents().iter().enumerate() {
                let parent_commit = self.read_commit(*parent, options.graph.max_object_size)?;
                if parent_commit.committer().timestamp() < cutoff {
                    continue;
                }
                let (generation, distance, tip_name) = if index == 0 {
                    (
                        current.generation + 1,
                        current.distance + 1,
                        current.tip_name.clone(),
                    )
                } else {
                    (
                        0,
                        current.distance.saturating_add(MERGE_TRAVERSAL_WEIGHT),
                        parent_name(&current, index + 1),
                    )
                };
                let candidate = CommitName {
                    tip_name,
                    tagger_date: current.tagger_date,
                    generation,
                    distance,
                    from_tag: current.from_tag,
                };
                if update_name(names, *parent, candidate) {
                    queued.push(*parent);
                }
            }
            stack.extend(queued.into_iter().rev());
        }
        Ok(())
    }
}

fn validate_options(options: &NameRevOptions) -> Result<()> {
    if options.abbreviation == 0 || options.abbreviation > ObjectId::HEX_LENGTH {
        return Err(Error::InvalidRepository(
            "name-rev abbreviation must be between 1 and 40".into(),
        ));
    }
    if options
        .include_patterns
        .iter()
        .chain(&options.exclude_patterns)
        .any(Vec::is_empty)
    {
        return Err(Error::InvalidRepository(
            "empty name-rev ref pattern".into(),
        ));
    }
    Ok(())
}

fn update_name(
    names: &mut HashMap<ObjectId, CommitName>,
    id: ObjectId,
    candidate: CommitName,
) -> bool {
    if names
        .get(&id)
        .is_some_and(|existing| !better_name(existing, &candidate))
    {
        return false;
    }
    names.insert(id, candidate);
    true
}

fn effective_distance(name: &CommitName) -> usize {
    name.distance
        .saturating_add(usize::from(name.generation > 0) * MERGE_TRAVERSAL_WEIGHT)
}

fn better_name(existing: &CommitName, candidate: &CommitName) -> bool {
    let old = effective_distance(existing);
    let new = effective_distance(candidate);
    if existing.from_tag && candidate.from_tag {
        return new < old;
    }
    if existing.from_tag != candidate.from_tag {
        return candidate.from_tag;
    }
    new < old || (new == old && candidate.tagger_date < existing.tagger_date)
}

fn render_name(name: &CommitName) -> String {
    if name.generation == 0 {
        return name.tip_name.clone();
    }
    format!(
        "{}~{}",
        name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name),
        name.generation
    )
}

fn parent_name(name: &CommitName, parent: usize) -> String {
    let tip = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
    if name.generation > 0 {
        format!("{tip}~{}^{parent}", name.generation)
    } else {
        format!("{tip}^{parent}")
    }
}

fn compare_tips(left: &Tip, right: &Tip) -> Ordering {
    right
        .from_tag
        .cmp(&left.from_tag)
        .then_with(|| left.tagger_date.cmp(&right.tagger_date))
        .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes()))
}

fn shorten_name(name: &str, shorten_tag: bool) -> String {
    if let Some(branch) = name.strip_prefix("refs/heads/") {
        return branch.to_owned();
    }
    if shorten_tag && let Some(tag) = name.strip_prefix("refs/tags/") {
        return tag.to_owned();
    }
    name.strip_prefix("refs/").unwrap_or(name).to_owned()
}

fn subpath_match(path: &[u8], pattern: &[u8]) -> Option<usize> {
    let mut offset = 0;
    loop {
        if wildmatch_ref(pattern, &path[offset..]) {
            return Some(offset);
        }
        let slash = path[offset..].iter().position(|byte| *byte == b'/')?;
        offset += slash + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, PreviousValue, ReferenceName, Signature,
        TagBuilder, Tree,
    };

    #[test]
    fn parent_notation_and_priority_helpers_follow_git_weights() {
        let main = CommitName {
            tip_name: "main".into(),
            tagger_date: 2,
            generation: 2,
            distance: 2,
            from_tag: false,
        };
        assert_eq!(render_name(&main), "main~2");
        assert_eq!(parent_name(&main, 2), "main~2^2");
        let tag = CommitName {
            tip_name: "tags/v1^0".into(),
            tagger_date: 3,
            generation: 1,
            distance: 1,
            from_tag: true,
        };
        assert_eq!(render_name(&tag), "tags/v1~1");
        assert!(better_name(&main, &tag));
        assert_eq!(subpath_match(b"refs/tags/v1", b"v*"), Some(10));
    }

    #[test]
    fn names_merge_parents_prefers_tags_and_handles_exact_objects() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let commit = |parents: &[ObjectId], timestamp| {
            let identity = Signature::new("N", "n@example.com", timestamp, 0).unwrap();
            let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
            for parent in parents {
                builder = builder.parent(*parent);
            }
            repository
                .write_commit(&builder.message(b"m\n".to_vec()).build())
                .unwrap()
        };
        let base = commit(&[], 1_000_000);
        let left = commit(&[base], 1_000_001);
        let side = commit(&[base], 1_000_002);
        let merge = commit(&[left, side], 1_000_003);
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                merge,
                PreviousValue::Any,
            )
            .unwrap();
        let tag = TagBuilder::new(
            left,
            ObjectKind::Commit,
            b"v1",
            Signature::new("T", "t@example.com", 1_000_004, 0).unwrap(),
        )
        .unwrap()
        .build();
        let (_, tag_id) = repository
            .create_annotated_tag("v1", &tag, false, 4096)
            .unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"blob").unwrap();
        repository
            .update_reference(
                &ReferenceName::new("refs/archive/blob").unwrap(),
                blob,
                PreviousValue::Any,
            )
            .unwrap();

        let targets = [merge, left, side, base, tag_id, blob];
        let named = repository
            .name_revs(&targets, &NameRevOptions::default())
            .unwrap();
        assert_eq!(named[0].name(), Some("main"));
        assert_eq!(named[1].name(), Some("tags/v1^0"));
        assert_eq!(named[2].name(), Some("main^2"));
        assert_eq!(named[3].name(), Some("tags/v1~1"));
        assert_eq!(named[4].name(), Some("tags/v1"));
        assert_eq!(named[5].name(), Some("archive/blob"));

        let tag_names = repository
            .name_revs(
                &[left, side],
                &NameRevOptions {
                    tags_only: true,
                    shorten_tags: true,
                    always: true,
                    allow_undefined: false,
                    ..NameRevOptions::default()
                },
            )
            .unwrap();
        assert_eq!(tag_names[0].name(), Some("v1^0"));
        assert!(tag_names[1].is_fallback());
    }
}
