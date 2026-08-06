//! Git-style revision name and suffix resolution.

use std::collections::BTreeSet;
use std::str::FromStr;

use crate::{Error, ObjectId, ObjectKind, PackIndex, Repository, Result};

/// Resource bounds for revision parsing and object discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionOptions {
    /// Minimum hexadecimal abbreviation length.
    pub min_abbreviation: usize,
    /// Maximum loose and packed object IDs inspected for an abbreviation.
    pub max_candidates: usize,
    /// Maximum annotated-tag chain depth.
    pub max_tag_depth: usize,
    /// Maximum bytes read from any object.
    pub max_object_size: usize,
    /// Maximum combined ancestry/peel suffix operations.
    pub max_suffix_operations: usize,
}

impl Default for RevisionOptions {
    fn default() -> Self {
        Self {
            min_abbreviation: 4,
            max_candidates: 10_000_000,
            max_tag_depth: 64,
            max_object_size: 1024 * 1024 * 1024,
            max_suffix_operations: 1_000_000,
        }
    }
}

/// The object ID and verified kind produced by a revision expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedObject {
    pub id: ObjectId,
    pub kind: ObjectKind,
}

impl Repository {
    /// Resolve a Git revision expression to an existing typed object.
    ///
    /// Base names support full IDs, unique abbreviated IDs, `@`, pseudorefs,
    /// fully-qualified refs, and DWIM tag/branch/remote names. Suffixes support
    /// `^`, `^N`, `~N`, `^{}`, `^{object|commit|tree|blob|tag}`, and `:path`.
    ///
    /// # Errors
    /// Returns an error for missing or ambiguous names, malformed expressions,
    /// wrong object types, graph/tag/resource limit violations, corrupt objects,
    /// or storage failures.
    pub fn resolve_revision(
        &self,
        expression: &str,
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        if expression.is_empty() || expression.as_bytes().contains(&0) {
            return revision_error("empty or NUL-containing expression");
        }
        validate_options(options)?;
        let split = expression
            .char_indices()
            .find_map(|(index, character)| matches!(character, '^' | '~' | ':').then_some(index))
            .unwrap_or(expression.len());
        if split == 0 {
            return revision_error("revision expression has no base name");
        }
        let mut resolved = self.resolve_revision_atom(&expression[..split], options)?;
        let suffix = expression.as_bytes();
        let mut cursor = split;
        let mut operations = 0usize;
        while cursor < suffix.len() {
            match suffix[cursor] {
                b'^' if suffix.get(cursor + 1) == Some(&b'{') => {
                    let close = suffix[cursor + 2..]
                        .iter()
                        .position(|byte| *byte == b'}')
                        .map(|offset| cursor + 2 + offset)
                        .ok_or_else(|| Error::InvalidRevision("unterminated peel suffix".into()))?;
                    let requested = &suffix[cursor + 2..close];
                    resolved = self.apply_peel(resolved, requested, options)?;
                    cursor = close + 1;
                    operations = checked_operations(operations, 1, options)?;
                }
                b'^' => {
                    cursor += 1;
                    let (parent, next) = decimal_suffix(suffix, cursor, 1)?;
                    cursor = next;
                    resolved = self.peel_to_kind(resolved, ObjectKind::Commit, options)?;
                    if parent != 0 {
                        let commit = self.read_commit(resolved.id, options.max_object_size)?;
                        let index = parent - 1;
                        let id = *commit.parents().get(index).ok_or_else(|| {
                            Error::InvalidRevision(format!("commit has no parent number {parent}"))
                        })?;
                        resolved = ResolvedObject {
                            id,
                            kind: ObjectKind::Commit,
                        };
                    }
                    operations = checked_operations(operations, 1, options)?;
                }
                b'~' => {
                    cursor += 1;
                    let (generations, next) = decimal_suffix(suffix, cursor, 1)?;
                    cursor = next;
                    resolved = self.peel_to_kind(resolved, ObjectKind::Commit, options)?;
                    operations = checked_operations(operations, generations, options)?;
                    for _ in 0..generations {
                        let commit = self.read_commit(resolved.id, options.max_object_size)?;
                        let id = *commit.parents().first().ok_or_else(|| {
                            Error::InvalidRevision("first-parent ancestry exceeds root".into())
                        })?;
                        resolved.id = id;
                    }
                }
                b':' => {
                    cursor += 1;
                    if cursor == suffix.len() {
                        resolved = self.treeish(resolved, options)?;
                    } else {
                        resolved = self.resolve_tree_path(resolved, &suffix[cursor..], options)?;
                    }
                    cursor = suffix.len();
                    operations = checked_operations(operations, 1, options)?;
                }
                _ => return revision_error("unexpected revision suffix"),
            }
        }
        Ok(resolved)
    }

    /// Resolve only the object ID portion of a revision expression.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::resolve_revision`].
    pub fn resolve_revision_id(
        &self,
        expression: &str,
        options: &RevisionOptions,
    ) -> Result<ObjectId> {
        Ok(self.resolve_revision(expression, options)?.id)
    }

    fn resolve_revision_atom(
        &self,
        atom: &str,
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        let atom = if atom == "@" { "HEAD" } else { atom };
        if atom.len() == ObjectId::HEX_LENGTH && atom.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let id = ObjectId::from_str(atom)?;
            return self.typed_existing(id, options.max_object_size);
        }
        if atom.len() >= options.min_abbreviation
            && atom.len() < ObjectId::HEX_LENGTH
            && atom.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let id = self.resolve_abbreviation(atom, options.max_candidates)?;
            return self.typed_existing(id, options.max_object_size);
        }
        if matches!(
            atom,
            "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        ) {
            let data = self.read_git_file(atom)?;
            let value = data.strip_suffix(b"\n").unwrap_or(&data);
            let value = std::str::from_utf8(value)
                .map_err(|_| Error::InvalidRevision(format!("{atom} is not ASCII")))?;
            let id = ObjectId::from_str(value)
                .map_err(|_| Error::InvalidRevision(format!("{atom} is invalid")))?;
            return self.typed_existing(id, options.max_object_size);
        }

        let candidates = dwim_candidates(atom);
        let mut matches = Vec::new();
        for candidate in candidates {
            match self.resolve_reference(&candidate) {
                Ok(id) => matches.push((candidate, id)),
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        matches.sort_unstable();
        matches.dedup();
        match matches.as_slice() {
            [] => Err(Error::InvalidRevision(format!("unknown revision `{atom}`"))),
            [(_, id)] => self.typed_existing(*id, options.max_object_size),
            _ => Err(Error::AmbiguousRevision(atom.to_owned())),
        }
    }

    fn resolve_abbreviation(&self, abbreviation: &str, max_candidates: usize) -> Result<ObjectId> {
        let prefix = abbreviation.to_ascii_lowercase();
        let mut matches = BTreeSet::new();
        let mut inspected = 0usize;
        self.collect_loose_prefix(&prefix, max_candidates, &mut inspected, &mut matches)?;
        self.collect_packed_prefix(&prefix, max_candidates, &mut inspected, &mut matches)?;
        match matches.len() {
            0 => Err(Error::InvalidRevision(format!(
                "unknown object abbreviation `{abbreviation}`"
            ))),
            1 => Ok(*matches.first().expect("one abbreviation match")),
            _ => Err(Error::AmbiguousRevision(abbreviation.to_owned())),
        }
    }

    fn collect_loose_prefix(
        &self,
        prefix: &str,
        limit: usize,
        inspected: &mut usize,
        matches: &mut BTreeSet<ObjectId>,
    ) -> Result<()> {
        let objects = self.git_path("objects");
        let directories = match self.filesystem().read_dir(&objects) {
            Ok(entries) => entries,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        for directory in directories {
            let Some(fanout) = directory.to_str() else {
                continue;
            };
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            if prefix.len() >= 2 && !prefix.starts_with(&fanout.to_ascii_lowercase()) {
                continue;
            }
            for file in self.filesystem().read_dir(&objects.join(&directory))? {
                let Some(rest) = file.to_str() else {
                    continue;
                };
                if rest.len() != 38 || !rest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                *inspected = inspected.checked_add(1).ok_or_else(|| {
                    Error::InvalidRevision("object candidate count overflow".into())
                })?;
                if *inspected > limit {
                    return revision_error("object abbreviation candidate limit exceeded");
                }
                let hex = format!("{fanout}{rest}").to_ascii_lowercase();
                if hex.starts_with(prefix) {
                    matches.insert(ObjectId::from_str(&hex)?);
                }
            }
        }
        Ok(())
    }

    fn collect_packed_prefix(
        &self,
        prefix: &str,
        limit: usize,
        inspected: &mut usize,
        matches: &mut BTreeSet<ObjectId>,
    ) -> Result<()> {
        let directory = self.git_path("objects/pack");
        let files = match self.filesystem().read_dir(&directory) {
            Ok(entries) => entries,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        for file in files {
            if file.extension().and_then(|value| value.to_str()) != Some("idx") {
                continue;
            }
            let index = PackIndex::parse(&self.filesystem().read(&directory.join(file))?)?;
            let prefix_bytes = prefix.as_bytes();
            let start = index
                .entries()
                .partition_point(|entry| entry.id.to_hex()[..prefix_bytes.len()] < *prefix_bytes);
            for entry in &index.entries()[start..] {
                let hex = entry.id.to_hex();
                if !hex.starts_with(prefix_bytes) {
                    break;
                }
                *inspected = inspected.checked_add(1).ok_or_else(|| {
                    Error::InvalidRevision("object candidate count overflow".into())
                })?;
                if *inspected > limit {
                    return revision_error("object abbreviation candidate limit exceeded");
                }
                matches.insert(entry.id);
            }
        }
        Ok(())
    }

    fn typed_existing(&self, id: ObjectId, max_size: usize) -> Result<ResolvedObject> {
        Ok(ResolvedObject {
            id,
            kind: self.read_object(id, max_size)?.kind(),
        })
    }

    fn apply_peel(
        &self,
        resolved: ResolvedObject,
        requested: &[u8],
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        match requested {
            b"" => self.peel_non_tag(resolved, options),
            b"object" => Ok(resolved),
            b"commit" => self.peel_to_kind(resolved, ObjectKind::Commit, options),
            b"tree" => self.treeish(resolved, options),
            b"blob" => self.peel_to_kind(resolved, ObjectKind::Blob, options),
            b"tag" if resolved.kind == ObjectKind::Tag => Ok(resolved),
            b"tag" => revision_error("object is not an annotated tag"),
            _ => revision_error("unknown peel type"),
        }
    }

    fn peel_non_tag(
        &self,
        resolved: ResolvedObject,
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        if resolved.kind != ObjectKind::Tag {
            return Ok(resolved);
        }
        let peeled = self.peel_tag(resolved.id, options.max_tag_depth, options.max_object_size)?;
        Ok(ResolvedObject {
            id: peeled.id,
            kind: peeled.kind,
        })
    }

    fn peel_to_kind(
        &self,
        resolved: ResolvedObject,
        expected: ObjectKind,
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        let resolved = self.peel_non_tag(resolved, options)?;
        if resolved.kind != expected {
            return Err(Error::InvalidRevision(format!(
                "expected {expected:?}, found {:?}",
                resolved.kind
            )));
        }
        Ok(resolved)
    }

    fn treeish(
        &self,
        resolved: ResolvedObject,
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        let resolved = self.peel_non_tag(resolved, options)?;
        match resolved.kind {
            ObjectKind::Tree => Ok(resolved),
            ObjectKind::Commit => {
                let id = self
                    .read_commit(resolved.id, options.max_object_size)?
                    .tree();
                Ok(ResolvedObject {
                    id,
                    kind: ObjectKind::Tree,
                })
            }
            _ => revision_error("object is not tree-ish"),
        }
    }

    fn resolve_tree_path(
        &self,
        resolved: ResolvedObject,
        path: &[u8],
        options: &RevisionOptions,
    ) -> Result<ResolvedObject> {
        let mut current = self.treeish(resolved, options)?;
        for component in path.split(|byte| *byte == b'/') {
            if component.is_empty() || matches!(component, b"." | b"..") {
                return revision_error("tree path has an empty or unsafe component");
            }
            if current.kind != ObjectKind::Tree {
                return revision_error("tree path traverses through a non-tree object");
            }
            let tree = self.read_tree(current.id, options.max_object_size)?;
            let entry = tree
                .entries()
                .iter()
                .find(|entry| entry.name() == component)
                .ok_or_else(|| {
                    Error::InvalidRevision(format!(
                        "path `{}` does not exist in tree",
                        String::from_utf8_lossy(path)
                    ))
                })?;
            current = ResolvedObject {
                id: entry.id(),
                kind: entry.mode().object_kind(),
            };
        }
        Ok(current)
    }
}

fn dwim_candidates(atom: &str) -> Vec<String> {
    if atom == "HEAD" || atom.starts_with("refs/") {
        return vec![atom.to_owned()];
    }
    [
        format!("refs/{atom}"),
        format!("refs/tags/{atom}"),
        format!("refs/heads/{atom}"),
        format!("refs/remotes/{atom}"),
        format!("refs/remotes/{atom}/HEAD"),
    ]
    .into_iter()
    .collect()
}

fn decimal_suffix(data: &[u8], start: usize, default: usize) -> Result<(usize, usize)> {
    let mut end = start;
    while data.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == start {
        return Ok((default, end));
    }
    let value = std::str::from_utf8(&data[start..end])
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| Error::InvalidRevision("ancestry count overflows usize".into()))?;
    Ok((value, end))
}

fn checked_operations(
    current: usize,
    additional: usize,
    options: &RevisionOptions,
) -> Result<usize> {
    let count = current
        .checked_add(additional)
        .ok_or_else(|| Error::InvalidRevision("suffix operation count overflow".into()))?;
    if count > options.max_suffix_operations {
        return revision_error("revision suffix operation limit exceeded");
    }
    Ok(count)
}

fn validate_options(options: &RevisionOptions) -> Result<()> {
    if options.min_abbreviation == 0 || options.min_abbreviation >= ObjectId::HEX_LENGTH {
        return revision_error("minimum abbreviation must be between 1 and 39");
    }
    if options.max_candidates == 0 || options.max_suffix_operations == 0 {
        return revision_error("revision resource limits must be nonzero");
    }
    Ok(())
}

fn revision_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRevision(message.into()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, PackOptions,
        PreviousValue, ReferenceName, Signature, TagBuilder, Tree, TreeEntry,
    };

    fn fixture() -> (
        Repository,
        MemoryFileSystem,
        Vec<ObjectId>,
        ObjectId,
        ObjectId,
    ) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Resolver", "resolver@example.com", 1, 0).unwrap();
        let nested_blob = repository
            .write_object(ObjectKind::Blob, b"nested contents")
            .unwrap();
        let nested_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), nested_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let root_blob = repository
            .write_object(ObjectKind::Blob, b"root contents")
            .unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"dir".to_vec(), nested_tree).unwrap(),
                    TreeEntry::new(EntryMode::Blob, b"root".to_vec(), root_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let root = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .message(b"root\n".to_vec())
                    .build(),
            )
            .unwrap();
        let first = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(root)
                    .message(b"first\n".to_vec())
                    .build(),
            )
            .unwrap();
        let side = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(root)
                    .message(b"side\n".to_vec())
                    .build(),
            )
            .unwrap();
        let merge = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(first)
                    .parent(side)
                    .message(b"merge\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                merge,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let tag = TagBuilder::new(merge, ObjectKind::Commit, "release", signature)
            .unwrap()
            .message(b"release\n".to_vec())
            .build();
        let tag_id = repository.write_tag(&tag, 4096).unwrap();
        repository
            .create_lightweight_tag("release", tag_id, false, 4096)
            .unwrap();
        (
            repository,
            filesystem,
            vec![root, first, side, merge],
            nested_blob,
            tag_id,
        )
    }

    #[test]
    fn resolves_dwim_refs_ancestry_peels_and_tree_paths() {
        let (repository, _, commits, nested_blob, tag_id) = fixture();
        let options = RevisionOptions {
            max_object_size: 4096,
            ..RevisionOptions::default()
        };
        let [root, first, side, merge] = commits.as_slice() else {
            panic!("fixture commit count");
        };
        assert_eq!(
            repository.resolve_revision_id("@", &options).unwrap(),
            *merge
        );
        assert_eq!(
            repository.resolve_revision_id("main^", &options).unwrap(),
            *first
        );
        assert_eq!(
            repository.resolve_revision_id("main^2", &options).unwrap(),
            *side
        );
        assert_eq!(
            repository.resolve_revision_id("main~2", &options).unwrap(),
            *root
        );
        assert_eq!(
            repository
                .resolve_revision("release^{tag}", &options)
                .unwrap(),
            ResolvedObject {
                id: tag_id,
                kind: ObjectKind::Tag
            }
        );
        assert_eq!(
            repository.resolve_revision("release^{}", &options).unwrap(),
            ResolvedObject {
                id: *merge,
                kind: ObjectKind::Commit
            }
        );
        assert_eq!(
            repository
                .resolve_revision("release^{tree}:dir/file", &options)
                .unwrap(),
            ResolvedObject {
                id: nested_blob,
                kind: ObjectKind::Blob
            }
        );
        assert!(repository.resolve_revision("main^3", &options).is_err());
        assert!(
            repository
                .resolve_revision("main^{blob}", &options)
                .is_err()
        );
        assert!(
            repository
                .resolve_revision("main:../file", &options)
                .is_err()
        );
    }

    #[test]
    fn resolves_unique_loose_and_packed_only_abbreviations() {
        let (repository, filesystem, commits, _, _) = fixture();
        let target = commits[3];
        let abbreviation = unique_prefix(target, &commits);
        assert_eq!(
            repository
                .resolve_revision_id(&abbreviation, &RevisionOptions::default())
                .unwrap(),
            target
        );

        let bundle = repository
            .write_pack(&commits, &PackOptions::default())
            .unwrap();
        assert_eq!(bundle.object_count, commits.len());
        for id in &commits {
            let hex = id.to_string();
            filesystem
                .remove_file(
                    &Path::new("repo/.git/objects")
                        .join(&hex[..2])
                        .join(&hex[2..]),
                )
                .unwrap();
        }
        assert_eq!(
            repository
                .resolve_revision_id(&abbreviation, &RevisionOptions::default())
                .unwrap(),
            target
        );
    }

    #[test]
    fn rejects_ambiguous_names_abbreviations_and_resource_exhaustion() {
        let (repository, filesystem, commits, _, _) = fixture();
        repository
            .create_lightweight_tag("main", commits[0], false, 4096)
            .unwrap();
        assert!(matches!(
            repository.resolve_revision("main", &RevisionOptions::default()),
            Err(Error::AmbiguousRevision(_))
        ));

        filesystem
            .create_dir_all(Path::new("repo/.git/objects/ab"))
            .unwrap();
        for suffix in [
            "cd000000000000000000000000000000000000",
            "cd111111111111111111111111111111111111",
        ] {
            filesystem
                .write(&Path::new("repo/.git/objects/ab").join(suffix), b"not read")
                .unwrap();
        }
        assert!(matches!(
            repository.resolve_revision("abcd", &RevisionOptions::default()),
            Err(Error::AmbiguousRevision(_))
        ));
        assert!(
            repository
                .resolve_revision(
                    "abcd",
                    &RevisionOptions {
                        max_candidates: 1,
                        ..RevisionOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            repository
                .resolve_revision(
                    "HEAD~2",
                    &RevisionOptions {
                        max_suffix_operations: 1,
                        ..RevisionOptions::default()
                    }
                )
                .is_err()
        );
    }

    fn unique_prefix(target: ObjectId, candidates: &[ObjectId]) -> String {
        let target = target.to_string();
        for length in 4..ObjectId::HEX_LENGTH {
            let prefix = &target[..length];
            if candidates
                .iter()
                .filter(|candidate| candidate.to_string().starts_with(prefix))
                .count()
                == 1
            {
                return prefix.to_owned();
            }
        }
        panic!("no unique prefix")
    }
}
