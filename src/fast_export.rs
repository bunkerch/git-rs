//! Bounded generation of Git fast-import streams.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Error, GraphOptions, ObjectId, ObjectKind, ReferenceName, Repository, Result,
    RevisionWalkOptions,
};

/// Selection and resource policy for fast-export generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastExportOptions {
    pub use_done_feature: bool,
    pub show_original_ids: bool,
    pub mark_tags: bool,
    pub initial_marks: BTreeMap<ObjectId, u64>,
    pub graph: GraphOptions,
    pub max_objects: usize,
    pub max_output_size: usize,
}

impl Default for FastExportOptions {
    fn default() -> Self {
        Self {
            use_done_feature: true,
            show_original_ids: false,
            mark_tags: true,
            initial_marks: BTreeMap::new(),
            graph: GraphOptions::default(),
            max_objects: 10_000_000,
            max_output_size: 16 * 1024 * 1024 * 1024,
        }
    }
}

/// Generated stream, persistent marks, and object counts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FastExportResult {
    stream: Vec<u8>,
    marks: BTreeMap<ObjectId, u64>,
    pub blobs: usize,
    pub commits: usize,
    pub tags: usize,
}

impl FastExportResult {
    #[must_use]
    pub fn stream(&self) -> &[u8] {
        &self.stream
    }

    #[must_use]
    pub const fn marks(&self) -> &BTreeMap<ObjectId, u64> {
        &self.marks
    }
}

impl Repository {
    /// Export the complete history and tag objects reachable from direct refs.
    ///
    /// The generated stream uses full-tree commits. This costs more bytes than
    /// parent diffs, but gives deterministic behavior and preserves merge
    /// commits without rename heuristics. Existing marks allow incremental
    /// exports without filesystem-based mark files.
    ///
    /// # Errors
    /// Returns an error for a symbolic input ref, unsupported commit metadata,
    /// corrupt/missing objects, duplicate marks, or a resource-limit breach.
    pub fn fast_export(
        &self,
        references: &[ReferenceName],
        options: &FastExportOptions,
    ) -> Result<FastExportResult> {
        Exporter::new(self, options)?.run(references)
    }
}

struct Exporter<'a> {
    repository: &'a Repository,
    options: &'a FastExportOptions,
    result: FastExportResult,
    next_mark: u64,
    objects: usize,
}

impl<'a> Exporter<'a> {
    fn new(repository: &'a Repository, options: &'a FastExportOptions) -> Result<Self> {
        if options.initial_marks.len() > options.max_objects {
            return export_error("initial fast-export mark count exceeds limit");
        }
        let mut used = BTreeSet::new();
        let mut next_mark = 1;
        for (id, mark) in &options.initial_marks {
            if *mark == 0 || !used.insert(*mark) {
                return export_error("fast-export marks must be unique and nonzero");
            }
            repository.read_object(*id, options.graph.max_object_size)?;
            next_mark = next_mark.max(
                mark.checked_add(1)
                    .ok_or_else(|| Error::InvalidRepository("fast-export mark overflow".into()))?,
            );
        }
        Ok(Self {
            repository,
            options,
            result: FastExportResult {
                marks: options.initial_marks.clone(),
                ..FastExportResult::default()
            },
            next_mark,
            objects: options.initial_marks.len(),
        })
    }

    fn run(mut self, references: &[ReferenceName]) -> Result<FastExportResult> {
        if references.is_empty() {
            return export_error("fast-export requires at least one reference");
        }
        let mut selected = Vec::with_capacity(references.len());
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for id in self.result.marks.keys() {
            if self
                .repository
                .read_object(*id, self.options.graph.max_object_size)?
                .kind()
                == ObjectKind::Commit
            {
                excludes.push(*id);
            }
        }
        for reference in references {
            let id = self.repository.resolve_reference(reference.as_str())?;
            let object = self
                .repository
                .read_object(id, self.options.graph.max_object_size)?;
            let commit = match object.kind() {
                ObjectKind::Commit => Some(id),
                ObjectKind::Tag => {
                    let peeled =
                        self.repository
                            .peel_tag(id, 64, self.options.graph.max_object_size)?;
                    (peeled.kind == ObjectKind::Commit).then_some(peeled.id)
                }
                ObjectKind::Blob | ObjectKind::Tree => None,
            };
            if let Some(commit) = commit {
                includes.push(commit);
            }
            selected.push((reference.clone(), id, object.kind()));
        }
        if self.options.use_done_feature {
            self.output(b"feature done\n")?;
        }
        let mut revisions = self.repository.walk_revisions(
            &includes,
            &excludes,
            &RevisionWalkOptions {
                graph: self.options.graph.clone(),
                ..RevisionWalkOptions::default()
            },
        )?;
        revisions.reverse();
        for revision in revisions {
            self.export_commit(revision.id(), revision.commit())?;
        }
        self.export_selected_tags(&selected)?;
        self.export_ref_resets(&selected)?;
        self.output(b"reset refs/heads/git-rs-fast-export\n\n")?;
        if self.options.use_done_feature {
            self.output(b"done\n")?;
        }
        Ok(self.result)
    }

    fn export_commit(&mut self, id: ObjectId, commit: &crate::Commit) -> Result<()> {
        let leaves = self
            .repository
            .flattened_tree(commit.tree(), self.options.graph.max_object_size)?;
        for leaf in &leaves {
            if leaf.raw_mode != 0o160_000 {
                self.export_blob(leaf.id)?;
            }
        }
        let mark = self.assign_mark(id)?;
        self.output(b"commit refs/heads/git-rs-fast-export\n")?;
        self.output(format!("mark :{mark}\n").as_bytes())?;
        if self.options.show_original_ids {
            self.output(format!("original-oid {id}\n").as_bytes())?;
        }
        self.output(format!("author {}\n", commit.author().encode()).as_bytes())?;
        self.output(format!("committer {}\n", commit.committer().encode()).as_bytes())?;
        for header in commit.extra_headers() {
            match header.name() {
                b"encoding" => {
                    self.output(b"encoding ")?;
                    self.output(header.value())?;
                    self.output(b"\n")?;
                }
                b"gpgsig" | b"gpgsig-sha256" => {
                    let algorithm = if header.name() == b"gpgsig" {
                        "sha1"
                    } else {
                        "sha256"
                    };
                    self.output(format!("gpgsig {algorithm} unknown\n").as_bytes())?;
                    self.data(header.value())?;
                }
                _ => return export_error("commit has unsupported extra header"),
            }
        }
        self.data(commit.message())?;
        for (index, parent) in commit.parents().iter().enumerate() {
            let parent_mark = self.result.marks.get(parent).copied().ok_or_else(|| {
                Error::InvalidRepository(format!("parent {parent} has no fast-export mark"))
            })?;
            let command = if index == 0 { "from" } else { "merge" };
            self.output(format!("{command} :{parent_mark}\n").as_bytes())?;
        }
        self.output(b"deleteall\n")?;
        for leaf in leaves {
            self.output(format!("M {:06o} ", leaf.raw_mode).as_bytes())?;
            if leaf.raw_mode == 0o160_000 {
                self.output(format!("{} ", leaf.id).as_bytes())?;
            } else {
                let blob_mark = self.result.marks[&leaf.id];
                self.output(format!(":{blob_mark} ").as_bytes())?;
            }
            self.quoted_path(&leaf.path)?;
            self.output(b"\n")?;
        }
        self.output(b"\n")?;
        self.result.commits += 1;
        Ok(())
    }

    fn export_blob(&mut self, id: ObjectId) -> Result<()> {
        if self.result.marks.contains_key(&id) {
            return Ok(());
        }
        let object = self
            .repository
            .read_object(id, self.options.graph.max_object_size)?;
        if object.kind() != ObjectKind::Blob {
            return export_error("tree leaf does not reference a blob");
        }
        let mark = self.assign_mark(id)?;
        self.output(b"blob\n")?;
        self.output(format!("mark :{mark}\n").as_bytes())?;
        if self.options.show_original_ids {
            self.output(format!("original-oid {id}\n").as_bytes())?;
        }
        self.data(object.data())?;
        self.result.blobs += 1;
        Ok(())
    }

    fn export_selected_tags(
        &mut self,
        selected: &[(ReferenceName, ObjectId, ObjectKind)],
    ) -> Result<()> {
        let mut pending = selected
            .iter()
            .filter(|(_, _, kind)| *kind == ObjectKind::Tag)
            .map(|(_, id, _)| *id)
            .filter(|id| !self.options.initial_marks.contains_key(id))
            .collect::<BTreeSet<_>>();
        while !pending.is_empty() {
            let mut progressed = false;
            for id in pending.clone() {
                let tag = self
                    .repository
                    .read_tag(id, self.options.graph.max_object_size)?;
                match tag.target_kind() {
                    ObjectKind::Blob => self.export_blob(tag.target())?,
                    ObjectKind::Commit | ObjectKind::Tag => {}
                    ObjectKind::Tree => return export_error("fast-export cannot mark a tree tag"),
                }
                let Some(target_mark) = self.result.marks.get(&tag.target()).copied() else {
                    continue;
                };
                if tag.target_kind() == ObjectKind::Tag && !self.options.mark_tags {
                    return export_error("nested tags require mark_tags");
                }
                let mark = if self.options.mark_tags {
                    Some(self.assign_mark(id)?)
                } else {
                    None
                };
                self.output(b"tag ")?;
                self.output(tag.name())?;
                self.output(b"\n")?;
                if let Some(mark) = mark {
                    self.output(format!("mark :{mark}\n").as_bytes())?;
                }
                self.output(format!("from :{target_mark}\n").as_bytes())?;
                if self.options.show_original_ids {
                    self.output(format!("original-oid {id}\n").as_bytes())?;
                }
                let tagger = tag
                    .tagger()
                    .ok_or_else(|| Error::InvalidRepository("tag has no tagger".into()))?;
                self.output(format!("tagger {}\n", tagger.encode()).as_bytes())?;
                if !tag.extra_headers().is_empty() {
                    return export_error("tag has unsupported extra headers");
                }
                self.data(tag.message())?;
                self.result.tags += 1;
                pending.remove(&id);
                progressed = true;
            }
            if !progressed {
                return export_error("tag target was not selected for export");
            }
        }
        Ok(())
    }

    fn export_ref_resets(
        &mut self,
        selected: &[(ReferenceName, ObjectId, ObjectKind)],
    ) -> Result<()> {
        for (reference, id, kind) in selected {
            if *kind == ObjectKind::Tag {
                let tag = self
                    .repository
                    .read_tag(*id, self.options.graph.max_object_size)?;
                let expected = format!("refs/tags/{}", String::from_utf8_lossy(tag.name()));
                if reference.as_str() != expected {
                    return export_error("annotated tag ref does not match its embedded name");
                }
                continue;
            }
            let mark = self.result.marks.get(id).copied().ok_or_else(|| {
                Error::InvalidRepository(format!("ref target {id} has no fast-export mark"))
            })?;
            self.output(format!("reset {reference}\nfrom :{mark}\n\n").as_bytes())?;
        }
        Ok(())
    }

    fn assign_mark(&mut self, id: ObjectId) -> Result<u64> {
        if let Some(mark) = self.result.marks.get(&id) {
            return Ok(*mark);
        }
        if self.objects >= self.options.max_objects {
            return export_error("fast-export object count exceeds limit");
        }
        let mark = self.next_mark;
        self.next_mark = mark
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRepository("fast-export mark overflow".into()))?;
        self.objects += 1;
        self.result.marks.insert(id, mark);
        Ok(mark)
    }

    fn data(&mut self, value: &[u8]) -> Result<()> {
        self.output(format!("data {}\n", value.len()).as_bytes())?;
        self.output(value)?;
        if value.ends_with(b"\n") {
            Ok(())
        } else {
            self.output(b"\n")
        }
    }

    fn quoted_path(&mut self, path: &[u8]) -> Result<()> {
        self.output(b"\"")?;
        for byte in path {
            match *byte {
                b'\\' => self.output(b"\\\\")?,
                b'"' => self.output(b"\\\"")?,
                b'\n' => self.output(b"\\n")?,
                b'\t' => self.output(b"\\t")?,
                0x20..=0x7e => self.output(&[*byte])?,
                byte => self.output(format!("\\{byte:03o}").as_bytes())?,
            }
        }
        self.output(b"\"")
    }

    fn output(&mut self, value: &[u8]) -> Result<()> {
        let size = self
            .result
            .stream
            .len()
            .checked_add(value.len())
            .ok_or_else(|| Error::InvalidRepository("fast-export output overflow".into()))?;
        if size > self.options.max_output_size {
            return export_error("fast-export output exceeds limit");
        }
        self.result.stream.extend_from_slice(value);
        Ok(())
    }
}

fn export_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(message.into()))
}

#[cfg(test)]
mod tests {
    use super::FastExportOptions;
    use crate::{FastImportOptions, InitOptions, MemoryFileSystem, ReferenceName, Repository};

    #[test]
    fn full_tree_export_round_trips_branches_merges_tags_and_quoted_paths() {
        let source = repository();
        source
            .fast_import(
                b"blob\nmark :1\ndata 5\nhello\n\
commit refs/heads/main\nmark :2\ncommitter A <a@b> 1 +0000\ndata 4\nroot\n\
M 100644 :1 \"dir/a b\"\n\
commit refs/heads/side\nmark :3\ncommitter A <a@b> 2 +0000\ndata 4\nside\nfrom :2\n\
M 100755 inline side\ndata 1\ns\n\
commit refs/heads/main\nmark :4\ncommitter A <a@b> 3 +0000\ndata 4\nmain\nfrom :2\n\
M 100644 inline main\ndata 1\nm\n\
commit refs/heads/main\nmark :5\ncommitter A <a@b> 4 +0000\ndata 5\nmerge\nfrom :4\nmerge :3\n\
M 100644 inline \"binary\\377\"\ndata 2\nx\0\n\
tag v1\nmark :6\nfrom :5\ntagger T <t@b> 5 +0000\ndata 3\ntag\n\
done\n",
                &FastImportOptions::default(),
            )
            .unwrap();
        let references = refs(&["refs/heads/main", "refs/heads/side", "refs/tags/v1"]);
        let exported = source
            .fast_export(
                &references,
                &FastExportOptions {
                    show_original_ids: true,
                    ..FastExportOptions::default()
                },
            )
            .unwrap();
        assert_eq!(exported.commits, 4);
        assert_eq!(exported.tags, 1);
        assert!(exported.stream().starts_with(b"feature done\n"));
        assert!(
            exported
                .stream()
                .windows(10)
                .any(|part| part == b"deleteall\n")
        );
        assert!(exported.stream().ends_with(b"done\n"));

        let destination = repository();
        destination
            .fast_import(
                exported.stream(),
                &FastImportOptions {
                    require_done: true,
                    ..FastImportOptions::default()
                },
            )
            .unwrap_or_else(|error| {
                panic!("{error:?}\n{}", String::from_utf8_lossy(exported.stream()))
            });
        for reference in references {
            assert_eq!(
                destination.resolve_reference(reference.as_str()).unwrap(),
                source.resolve_reference(reference.as_str()).unwrap()
            );
        }
        assert!(
            destination
                .resolve_reference("refs/heads/git-rs-fast-export")
                .is_err()
        );
    }

    #[test]
    fn validates_marks_and_bounds_output() {
        let source = repository();
        source
            .fast_import(
                b"commit refs/heads/main\nmark :1\ncommitter A <a@b> 1 +0000\ndata 0\ndone\n",
                &FastImportOptions::default(),
            )
            .unwrap();
        let references = refs(&["refs/heads/main"]);
        assert!(
            source
                .fast_export(
                    &references,
                    &FastExportOptions {
                        max_output_size: 1,
                        ..FastExportOptions::default()
                    }
                )
                .is_err()
        );
        let id = source.resolve_reference("refs/heads/main").unwrap();
        let mut initial_marks = std::collections::BTreeMap::new();
        initial_marks.insert(id, 0);
        assert!(
            source
                .fast_export(
                    &references,
                    &FastExportOptions {
                        initial_marks,
                        ..FastExportOptions::default()
                    }
                )
                .is_err()
        );
    }

    fn refs(names: &[&str]) -> Vec<ReferenceName> {
        names
            .iter()
            .map(|name| ReferenceName::new(*name).unwrap())
            .collect()
    }

    fn repository() -> Repository {
        Repository::init(
            MemoryFileSystem::new(),
            ".",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap()
    }
}
