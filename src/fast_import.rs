//! Bounded ingestion of Git fast-import byte streams.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use crate::{
    CommitBuilder, Error, ExtraHeader, GraphOptions, Index, IndexEntry, IndexVersion, ObjectId,
    ObjectKind, PreviousValue, ReferenceEdit, ReferenceName, Repository, Result, Signature,
    StatData, TagBuilder,
};

/// Resource and publication policy for a fast-import stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastImportOptions {
    pub force: bool,
    pub require_done: bool,
    pub initial_marks: BTreeMap<u64, ObjectId>,
    pub max_stream_size: usize,
    pub max_commands: usize,
    pub max_data_size: usize,
    pub max_total_data: usize,
    pub max_marks: usize,
    pub max_paths: usize,
    pub max_file_changes: usize,
    pub max_parents: usize,
    pub max_object_size: usize,
    pub max_response_size: usize,
    pub graph: GraphOptions,
}

impl Default for FastImportOptions {
    fn default() -> Self {
        Self {
            force: false,
            require_done: false,
            initial_marks: BTreeMap::new(),
            max_stream_size: 16 * 1024 * 1024 * 1024,
            max_commands: 10_000_000,
            max_data_size: 1024 * 1024 * 1024,
            max_total_data: 16 * 1024 * 1024 * 1024,
            max_marks: 10_000_000,
            max_paths: 10_000_000,
            max_file_changes: 10_000_000,
            max_parents: 1_000_000,
            max_object_size: 1024 * 1024 * 1024,
            max_response_size: 1024 * 1024 * 1024,
            graph: GraphOptions::default(),
        }
    }
}

/// Objects, marks, responses, and ref updates produced by an import.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FastImportResult {
    marks: BTreeMap<u64, ObjectId>,
    responses: Vec<u8>,
    pub blobs: usize,
    pub commits: usize,
    pub tags: usize,
    pub ref_updates: usize,
    pub checkpoints: usize,
}

impl FastImportResult {
    #[must_use]
    pub const fn marks(&self) -> &BTreeMap<u64, ObjectId> {
        &self.marks
    }

    #[must_use]
    pub fn responses(&self) -> &[u8] {
        &self.responses
    }
}

impl Repository {
    /// Parse and apply a complete Git fast-import stream.
    ///
    /// Immutable objects may be written while parsing. Refs are retained in an
    /// in-memory branch table and published atomically at checkpoints and
    /// successful completion, so malformed commands never expose partial ref
    /// updates after the most recent checkpoint.
    ///
    /// # Errors
    /// Returns an error for malformed/unsupported commands, invalid objects,
    /// non-fast-forward publication without force, or exceeded resource limits.
    pub fn fast_import(
        &self,
        stream: &[u8],
        options: &FastImportOptions,
    ) -> Result<FastImportResult> {
        if stream.len() > options.max_stream_size {
            return import_error("fast-import stream exceeds limit");
        }
        if options.initial_marks.len() > options.max_marks {
            return import_error("initial fast-import mark count exceeds limit");
        }
        for id in options.initial_marks.values() {
            self.read_object(*id, options.max_object_size)?;
        }
        let mut parser = ImportParser::new(self, stream, options.clone());
        parser.run()?;
        Ok(parser.result)
    }
}

struct ImportParser<'a> {
    repository: &'a Repository,
    cursor: StreamCursor<'a>,
    options: FastImportOptions,
    result: FastImportResult,
    branches: BTreeMap<String, Option<ObjectId>>,
    published: BTreeMap<String, Option<ObjectId>>,
    dirty_refs: BTreeSet<String>,
    commands: usize,
    total_data: usize,
    saw_done: bool,
    options_allowed: bool,
}

impl<'a> ImportParser<'a> {
    fn new(repository: &'a Repository, stream: &'a [u8], options: FastImportOptions) -> Self {
        let marks = options.initial_marks.clone();
        Self {
            repository,
            cursor: StreamCursor::new(stream),
            options,
            result: FastImportResult {
                marks,
                ..FastImportResult::default()
            },
            branches: BTreeMap::new(),
            published: BTreeMap::new(),
            dirty_refs: BTreeSet::new(),
            commands: 0,
            total_data: 0,
            saw_done: false,
            options_allowed: true,
        }
    }

    fn run(&mut self) -> Result<()> {
        while let Some(line) = self.cursor.take_line() {
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            self.bump_command()?;
            if line == b"blob" {
                self.options_allowed = false;
                self.parse_blob()?;
            } else if let Some(name) = line.strip_prefix(b"commit ") {
                self.options_allowed = false;
                self.parse_commit(parse_ref(name)?)?;
            } else if let Some(name) = line.strip_prefix(b"tag ") {
                self.options_allowed = false;
                self.parse_tag(parse_text(name, "tag name")?)?;
            } else if let Some(name) = line.strip_prefix(b"reset ") {
                self.options_allowed = false;
                self.parse_reset(parse_ref(name)?)?;
            } else if line == b"alias" {
                self.options_allowed = false;
                self.parse_alias()?;
            } else if line == b"checkpoint" {
                self.options_allowed = false;
                self.publish_refs()?;
                self.result.checkpoints += 1;
            } else if let Some(message) = line.strip_prefix(b"progress ") {
                self.push_response(b"progress ")?;
                self.push_response(message)?;
                self.push_response(b"\n")?;
            } else if let Some(mark) = line.strip_prefix(b"get-mark ") {
                let id = self.resolve_mark(mark)?;
                self.push_response(format!("{id}\n").as_bytes())?;
            } else if let Some(reference) = line.strip_prefix(b"cat-blob ") {
                self.respond_cat_blob(reference)?;
            } else if let Some(arguments) = line.strip_prefix(b"ls ") {
                self.respond_ls_named(arguments)?;
            } else if let Some(feature) = line.strip_prefix(b"feature ") {
                self.parse_feature(feature)?;
            } else if let Some(option) = line.strip_prefix(b"option ") {
                self.parse_option(option)?;
            } else if line == b"done" {
                self.saw_done = true;
                break;
            } else {
                return import_error(format!(
                    "unsupported fast-import command `{}`",
                    String::from_utf8_lossy(line)
                ));
            }
        }
        if self.options.require_done && !self.saw_done {
            return import_error("fast-import stream ended without required done command");
        }
        self.publish_refs()?;
        Ok(())
    }

    fn parse_blob(&mut self) -> Result<()> {
        let mark = self.take_optional_mark()?;
        self.take_optional_original_oid();
        let data = self.take_data()?;
        let id = self.repository.write_object(ObjectKind::Blob, &data)?;
        if let Some(mark) = mark {
            self.set_mark(mark, id)?;
        }
        self.result.blobs += 1;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn parse_commit(&mut self, reference: String) -> Result<()> {
        let mark = self.take_optional_mark()?;
        self.take_optional_original_oid();
        let author = self.take_optional_identity(b"author ")?;
        let committer = self.take_required_identity(b"committer ")?;
        let mut extra_headers = Vec::new();
        while let Some(line) = self.cursor.peek_line() {
            if let Some(arguments) = line.strip_prefix(b"gpgsig ") {
                let arguments = split_once(arguments, b' ', "gpgsig arguments")?;
                let name = match arguments.0 {
                    b"sha1" => b"gpgsig".as_slice(),
                    b"sha256" => b"gpgsig-sha256".as_slice(),
                    _ => return import_error("unsupported gpgsig hash algorithm"),
                };
                self.cursor.take_line();
                let signature = self.take_data()?;
                extra_headers.push(ExtraHeader::new(name.to_vec(), signature)?);
            } else if let Some(encoding) = line.strip_prefix(b"encoding ") {
                self.cursor.take_line();
                extra_headers.push(ExtraHeader::new(b"encoding".to_vec(), encoding.to_vec())?);
            } else {
                break;
            }
        }
        let message = self.take_data()?;

        let existing = self.branch_tip(&reference)?;
        let mut explicit_from = None;
        let mut merges = Vec::new();
        let mut changes = Vec::new();
        let mut change_count = 0usize;
        while let Some(line) = self.cursor.peek_line() {
            if line.is_empty() {
                self.cursor.take_line();
                break;
            }
            if let Some(value) = line.strip_prefix(b"from ") {
                if explicit_from.is_some() {
                    return import_error("commit has multiple from commands");
                }
                explicit_from = Some(self.resolve_commitish(value)?);
                self.cursor.take_line();
            } else if let Some(value) = line.strip_prefix(b"merge ") {
                merges.push(self.resolve_commitish(value)?);
                if merges.len() > self.options.max_parents {
                    return import_error("fast-import parent count exceeds limit");
                }
                self.cursor.take_line();
            } else if is_file_command(line) {
                let line = self.cursor.take_line().expect("peeked command").to_vec();
                changes.push(self.parse_file_command(&line)?);
                change_count += 1;
                if change_count > self.options.max_file_changes {
                    return import_error("fast-import file change count exceeds limit");
                }
            } else if let Some(value) = line.strip_prefix(b"get-mark ") {
                let value = value.to_vec();
                self.cursor.take_line();
                let id = self.resolve_mark(&value)?;
                self.push_response(format!("{id}\n").as_bytes())?;
            } else if let Some(value) = line.strip_prefix(b"cat-blob ") {
                let value = value.to_vec();
                self.cursor.take_line();
                self.respond_cat_blob(&value)?;
            } else if let Some(arguments) = line.strip_prefix(b"ls ") {
                let arguments = arguments.to_vec();
                self.cursor.take_line();
                if arguments.starts_with(b"\"") {
                    let path = parse_path(&arguments)?;
                    let mut files = if let Some(base) = explicit_from.or(existing) {
                        self.files_from_commit(base)?
                    } else {
                        BTreeMap::new()
                    };
                    for change in changes.iter().cloned() {
                        self.apply_file_change(&mut files, change)?;
                    }
                    self.respond_ls_active(&files, &path)?;
                } else {
                    self.respond_ls_named(&arguments)?;
                }
            } else {
                break;
            }
        }

        let first_parent = explicit_from.or(existing);
        let mut parents = Vec::new();
        if let Some(parent) = first_parent {
            parents.push(parent);
        } else if let Some(parent) = merges.first().copied() {
            parents.push(parent);
            merges.remove(0);
        }
        parents.extend(merges);
        if parents.len() > self.options.max_parents {
            return import_error("fast-import parent count exceeds limit");
        }
        let base_tree = explicit_from.or(existing);
        let mut files = if let Some(base) = base_tree {
            self.files_from_commit(base)?
        } else {
            BTreeMap::new()
        };
        for change in changes {
            self.apply_file_change(&mut files, change)?;
        }
        if files.len() > self.options.max_paths {
            return import_error("fast-import tree path count exceeds limit");
        }
        let tree = self.write_flat_tree(&files)?;
        let author = author.unwrap_or_else(|| committer.clone());
        let mut builder = CommitBuilder::new(tree, author, committer).message(message);
        for parent in parents {
            builder = builder.parent(parent);
        }
        for header in extra_headers {
            builder = builder.extra_header(header);
        }
        let id = self.repository.write_commit(&builder.build())?;
        self.set_branch(reference, Some(id))?;
        if let Some(mark) = mark {
            self.set_mark(mark, id)?;
        }
        self.result.commits += 1;
        Ok(())
    }

    fn parse_tag(&mut self, name: &str) -> Result<()> {
        let mark = self.take_optional_mark()?;
        self.take_optional_original_oid();
        let from = self.take_prefixed_line(b"from ", "tag requires from")?;
        let target = self.resolve_commitish(&from)?;
        let tagger = self.take_required_identity(b"tagger ")?;
        let message = self.take_data()?;
        let object = self
            .repository
            .read_object(target, self.options.max_object_size)?;
        let tag = TagBuilder::new(target, object.kind(), name.as_bytes(), tagger)?
            .message(message)
            .build();
        let id = self
            .repository
            .write_tag(&tag, self.options.max_object_size)?;
        self.set_branch(format!("refs/tags/{name}"), Some(id))?;
        if let Some(mark) = mark {
            self.set_mark(mark, id)?;
        }
        self.result.tags += 1;
        Ok(())
    }

    fn parse_reset(&mut self, reference: String) -> Result<()> {
        let target = if let Some(line) = self.cursor.peek_line() {
            if let Some(value) = line.strip_prefix(b"from ") {
                let value = value.to_vec();
                self.cursor.take_line();
                let id = self.resolve_commitish(&value)?;
                (!id.is_null()).then_some(id)
            } else {
                None
            }
        } else {
            None
        };
        if self.cursor.peek_line() == Some(b"".as_slice()) {
            self.cursor.take_line();
        }
        self.set_branch(reference, target)
    }

    fn parse_alias(&mut self) -> Result<()> {
        let mark_line = self.take_prefixed_line(b"mark ", "alias requires mark")?;
        let mark = parse_mark(&mark_line)?;
        let target = self.take_prefixed_line(b"to ", "alias requires to")?;
        let id = self.resolve_commitish(&target)?;
        self.set_mark(mark, id)
    }

    fn parse_file_command(&mut self, line: &[u8]) -> Result<FileChange> {
        if line == b"deleteall" {
            return Ok(FileChange::DeleteAll);
        }
        if let Some(path) = line.strip_prefix(b"D ") {
            return Ok(FileChange::Delete(parse_path(path)?));
        }
        if let Some(arguments) = line.strip_prefix(b"M ") {
            let (mode, rest) = take_token(arguments, "file mode")?;
            let (reference, path) = take_token(rest, "file dataref")?;
            let mode = parse_mode(mode)?;
            let path = parse_path(path)?;
            let id = if reference == b"inline" {
                let data = self.take_data()?;
                self.repository.write_object(ObjectKind::Blob, &data)?
            } else {
                self.resolve_dataref(reference)?
            };
            return Ok(FileChange::Modify { mode, id, path });
        }
        if let Some(arguments) = line.strip_prefix(b"C ") {
            let (source, destination) = parse_two_paths(arguments)?;
            return Ok(FileChange::Copy {
                source,
                destination,
            });
        }
        if let Some(arguments) = line.strip_prefix(b"R ") {
            let (source, destination) = parse_two_paths(arguments)?;
            return Ok(FileChange::Rename {
                source,
                destination,
            });
        }
        if let Some(arguments) = line.strip_prefix(b"N ") {
            let (reference, target) = take_token(arguments, "note dataref")?;
            let target = self.resolve_commitish(target)?;
            let id = if reference == b"inline" {
                let data = self.take_data()?;
                self.repository.write_object(ObjectKind::Blob, &data)?
            } else {
                self.resolve_dataref(reference)?
            };
            return Ok(FileChange::Modify {
                mode: 0o100_644,
                id,
                path: target.to_string().into_bytes(),
            });
        }
        import_error("malformed fast-import file command")
    }

    fn apply_file_change(
        &self,
        files: &mut BTreeMap<Vec<u8>, (u32, ObjectId)>,
        change: FileChange,
    ) -> Result<()> {
        match change {
            FileChange::DeleteAll => files.clear(),
            FileChange::Delete(path) => delete_path(files, &path),
            FileChange::Modify { mode, id, path } => {
                delete_path(files, &path);
                let expected = mode_kind(mode);
                let object = self
                    .repository
                    .read_object(id, self.options.max_object_size)?;
                if mode == 0o040_000 {
                    if object.kind() != ObjectKind::Tree {
                        return import_error("040000 filemodify does not reference a tree");
                    }
                    for leaf in self
                        .repository
                        .flattened_tree(id, self.options.max_object_size)?
                    {
                        let nested = join_import_path(&path, &leaf.path)?;
                        files.insert(nested, (leaf.raw_mode, leaf.id));
                    }
                } else {
                    if object.kind() != expected {
                        return import_error("filemodify references the wrong object type");
                    }
                    files.insert(path, (mode, id));
                }
            }
            FileChange::Copy {
                source,
                destination,
            } => copy_path(files, &source, &destination, false)?,
            FileChange::Rename {
                source,
                destination,
            } => copy_path(files, &source, &destination, true)?,
        }
        Ok(())
    }

    fn files_from_commit(&self, id: ObjectId) -> Result<BTreeMap<Vec<u8>, (u32, ObjectId)>> {
        let commit = self
            .repository
            .read_commit(id, self.options.max_object_size)?;
        Ok(self
            .repository
            .flattened_tree(commit.tree(), self.options.max_object_size)?
            .into_iter()
            .map(|leaf| (leaf.path, (leaf.raw_mode, leaf.id)))
            .collect())
    }

    fn write_flat_tree(&self, files: &BTreeMap<Vec<u8>, (u32, ObjectId)>) -> Result<ObjectId> {
        let entries = files
            .iter()
            .map(|(path, (mode, id))| {
                IndexEntry::new(path.clone(), *mode, *id, StatData::default())
            })
            .collect::<Result<Vec<_>>>()?;
        self.repository
            .write_index_tree(&Index::new(IndexVersion::V2, entries)?)
    }

    fn take_optional_mark(&mut self) -> Result<Option<u64>> {
        let Some(line) = self.cursor.peek_line() else {
            return Ok(None);
        };
        let Some(value) = line.strip_prefix(b"mark ") else {
            return Ok(None);
        };
        let value = value.to_vec();
        self.cursor.take_line();
        Ok(Some(parse_mark(&value)?))
    }

    fn take_optional_original_oid(&mut self) {
        if self
            .cursor
            .peek_line()
            .is_some_and(|line| line.starts_with(b"original-oid "))
        {
            self.cursor.take_line();
        }
    }

    fn take_optional_identity(&mut self, prefix: &[u8]) -> Result<Option<Signature>> {
        let Some(line) = self.cursor.peek_line() else {
            return Ok(None);
        };
        let Some(value) = line.strip_prefix(prefix) else {
            return Ok(None);
        };
        let value = value.to_vec();
        self.cursor.take_line();
        Ok(Some(Signature::parse(&value)?))
    }

    fn take_required_identity(&mut self, prefix: &[u8]) -> Result<Signature> {
        self.take_optional_identity(prefix)?
            .ok_or_else(|| Error::InvalidRepository("fast-import identity command missing".into()))
    }

    fn take_prefixed_line(&mut self, prefix: &[u8], message: &str) -> Result<Vec<u8>> {
        let line = self
            .cursor
            .take_line()
            .ok_or_else(|| Error::InvalidRepository(message.into()))?;
        line.strip_prefix(prefix)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::InvalidRepository(message.into()))
    }

    fn take_data(&mut self) -> Result<Vec<u8>> {
        let line = self
            .cursor
            .take_line()
            .ok_or_else(|| Error::InvalidRepository("fast-import data command missing".into()))?;
        let argument = line
            .strip_prefix(b"data ")
            .ok_or_else(|| Error::InvalidRepository("expected fast-import data command".into()))?;
        let data = if let Some(delimiter) = argument.strip_prefix(b"<<") {
            if delimiter.is_empty() {
                return import_error("empty fast-import data delimiter");
            }
            self.cursor.take_delimited(delimiter)?
        } else {
            let size = parse_decimal(argument, "data size")?;
            if size > self.options.max_data_size {
                return import_error("fast-import data block exceeds limit");
            }
            self.cursor.take_bytes(size)?.to_vec()
        };
        if data.len() > self.options.max_data_size {
            return import_error("fast-import data block exceeds limit");
        }
        self.total_data = self
            .total_data
            .checked_add(data.len())
            .ok_or_else(|| Error::InvalidRepository("fast-import data size overflow".into()))?;
        if self.total_data > self.options.max_total_data {
            return import_error("fast-import total data exceeds limit");
        }
        Ok(data)
    }

    fn resolve_mark(&self, value: &[u8]) -> Result<ObjectId> {
        let mark = parse_mark(value)?;
        self.result
            .marks
            .get(&mark)
            .copied()
            .ok_or_else(|| Error::InvalidRepository(format!("undefined fast-import mark :{mark}")))
    }

    fn resolve_dataref(&mut self, value: &[u8]) -> Result<ObjectId> {
        if value.starts_with(b":") {
            self.resolve_mark(value)
        } else {
            ObjectId::from_str(parse_text(value, "object ID")?)
        }
    }

    fn resolve_commitish(&mut self, value: &[u8]) -> Result<ObjectId> {
        if value.starts_with(b":") {
            let id = self.resolve_mark(value)?;
            return self.peel_commitish(id);
        }
        let text = parse_text(value, "commit-ish")?;
        if !text.ends_with("^0")
            && let Some(value) = self.branches.get(text)
        {
            let id = value.ok_or_else(|| {
                Error::InvalidRepository(format!("deleted fast-import branch {text}"))
            })?;
            return self.peel_commitish(id);
        }
        if !text.ends_with("^0")
            && let Some(id) = self.load_branch(text)?
        {
            return self.peel_commitish(id);
        }
        let id = self.repository.resolve_revision_id(
            text,
            &crate::RevisionOptions {
                max_object_size: self.options.max_object_size,
                ..crate::RevisionOptions::default()
            },
        )?;
        self.peel_commitish(id)
    }

    fn peel_commitish(&self, id: ObjectId) -> Result<ObjectId> {
        let object = self
            .repository
            .read_object(id, self.options.max_object_size)?;
        match object.kind() {
            ObjectKind::Commit => Ok(id),
            ObjectKind::Tag => {
                let peeled = self
                    .repository
                    .peel_tag(id, 64, self.options.max_object_size)?;
                if peeled.kind == ObjectKind::Commit {
                    Ok(peeled.id)
                } else {
                    import_error("commit-ish does not resolve to a commit")
                }
            }
            ObjectKind::Blob | ObjectKind::Tree => {
                import_error("commit-ish does not resolve to a commit")
            }
        }
    }

    fn set_mark(&mut self, mark: u64, id: ObjectId) -> Result<()> {
        if !self.result.marks.contains_key(&mark)
            && self.result.marks.len() >= self.options.max_marks
        {
            return import_error("fast-import mark count exceeds limit");
        }
        self.result.marks.insert(mark, id);
        Ok(())
    }

    fn branch_tip(&mut self, name: &str) -> Result<Option<ObjectId>> {
        if let Some(value) = self.branches.get(name) {
            return Ok(*value);
        }
        self.load_branch(name)
    }

    fn load_branch(&mut self, name: &str) -> Result<Option<ObjectId>> {
        let value = match self.repository.resolve_reference(name) {
            Ok(id) => Some(id),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        self.branches.insert(name.to_owned(), value);
        self.published.insert(name.to_owned(), value);
        Ok(value)
    }

    fn set_branch(&mut self, name: String, value: Option<ObjectId>) -> Result<()> {
        ReferenceName::new(name.clone())?;
        if !self.branches.contains_key(&name) {
            self.load_branch(&name)?;
        }
        self.branches.insert(name.clone(), value);
        self.dirty_refs.insert(name);
        Ok(())
    }

    fn publish_refs(&mut self) -> Result<()> {
        if self.dirty_refs.is_empty() {
            return Ok(());
        }
        let mut edits = Vec::new();
        for name in &self.dirty_refs {
            let old = self.published[name];
            let new = self.branches[name];
            if old == new {
                continue;
            }
            let reference = ReferenceName::new(name.clone())?;
            match (old, new) {
                (Some(old), Some(new)) => {
                    if !self.options.force && name.starts_with("refs/heads/") {
                        if !self.repository.is_ancestor(old, new, &self.options.graph)? {
                            return import_error(format!(
                                "non-fast-forward fast-import update for {name}"
                            ));
                        }
                    } else if !self.options.force && name.starts_with("refs/tags/") {
                        return import_error(format!(
                            "fast-import refuses to replace existing tag {name}"
                        ));
                    }
                    edits.push(ReferenceEdit::update(
                        reference,
                        new,
                        PreviousValue::MustExist(old),
                    ));
                }
                (None, Some(new)) => edits.push(ReferenceEdit::update(
                    reference,
                    new,
                    PreviousValue::MustNotExist,
                )),
                (Some(old), None) => edits.push(ReferenceEdit::delete(reference, old)),
                (None, None) => {}
            }
        }
        self.repository.apply_reference_transaction(&edits)?;
        self.result.ref_updates += edits.len();
        for name in &self.dirty_refs {
            self.published.insert(name.clone(), self.branches[name]);
        }
        self.dirty_refs.clear();
        Ok(())
    }

    fn parse_feature(&mut self, feature: &[u8]) -> Result<()> {
        match feature {
            b"done" => self.options.require_done = true,
            b"force" => self.options.force = true,
            b"notes"
            | b"get-mark"
            | b"cat-blob"
            | b"ls"
            | b"date-format=raw"
            | b"date-format=raw-permissive" => {}
            _ => {
                return import_error(format!(
                    "unsupported fast-import feature `{}`",
                    String::from_utf8_lossy(feature)
                ));
            }
        }
        Ok(())
    }

    fn parse_option(&mut self, option: &[u8]) -> Result<()> {
        if !self.options_allowed {
            return import_error("fast-import option appears after data commands");
        }
        let name = option.split(|byte| *byte == b'=').next().unwrap_or(option);
        if matches!(
            name,
            b"quiet"
                | b"stats"
                | b"depth"
                | b"active-branches"
                | b"big-file-threshold"
                | b"max-pack-size"
                | b"export-pack-edges"
        ) {
            Ok(())
        } else {
            import_error("unsupported or semantic fast-import option")
        }
    }

    fn respond_cat_blob(&mut self, reference: &[u8]) -> Result<()> {
        let id = self.resolve_dataref(reference)?;
        let object = self
            .repository
            .read_object(id, self.options.max_object_size)?;
        if object.kind() != ObjectKind::Blob {
            return import_error("cat-blob reference is not a blob");
        }
        self.push_response(format!("{id} blob {}\n", object.data().len()).as_bytes())?;
        self.push_response(object.data())?;
        self.push_response(b"\n")
    }

    fn respond_ls_named(&mut self, arguments: &[u8]) -> Result<()> {
        let (reference, path) = take_token(arguments, "ls tree-ish")?;
        let id = self.resolve_dataref(reference)?;
        let path = parse_path(path)?;
        let resolved = self.resolve_ls_path(id, &path)?;
        self.write_ls_response(resolved, &path)
    }

    fn respond_ls_active(
        &mut self,
        files: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
        path: &[u8],
    ) -> Result<()> {
        if let Some(entry) = files.get(path).copied() {
            return self.write_ls_response(Some(entry), path);
        }
        let mut prefix = path.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let subtree = files
            .iter()
            .filter_map(|(candidate, entry)| {
                candidate
                    .strip_prefix(prefix.as_slice())
                    .map(|relative| (relative.to_vec(), *entry))
            })
            .collect::<BTreeMap<_, _>>();
        if subtree.is_empty() {
            self.write_ls_response(None, path)
        } else {
            let id = self.write_flat_tree(&subtree)?;
            self.write_ls_response(Some((0o040_000, id)), path)
        }
    }

    fn resolve_ls_path(&self, mut id: ObjectId, path: &[u8]) -> Result<Option<(u32, ObjectId)>> {
        let object = self
            .repository
            .read_object(id, self.options.max_object_size)?;
        id = match object.kind() {
            ObjectKind::Commit => self
                .repository
                .read_commit(id, self.options.max_object_size)?
                .tree(),
            ObjectKind::Tree => id,
            ObjectKind::Tag => {
                let peeled = self
                    .repository
                    .peel_tag(id, 64, self.options.max_object_size)?;
                match peeled.kind {
                    ObjectKind::Commit => self
                        .repository
                        .read_commit(peeled.id, self.options.max_object_size)?
                        .tree(),
                    ObjectKind::Tree => peeled.id,
                    ObjectKind::Blob | ObjectKind::Tag => {
                        return import_error("ls root is not tree-ish");
                    }
                }
            }
            ObjectKind::Blob => return import_error("ls root is not tree-ish"),
        };
        if path.is_empty() {
            return Ok(Some((0o040_000, id)));
        }
        let mut components = path.split(|byte| *byte == b'/').peekable();
        while let Some(component) = components.next() {
            let tree = self
                .repository
                .read_tree(id, self.options.max_object_size)?;
            let Some(entry) = tree
                .entries()
                .iter()
                .find(|entry| entry.name() == component)
            else {
                return Ok(None);
            };
            if components.peek().is_none() {
                return Ok(Some((mode_number(entry.mode()), entry.id())));
            }
            if entry.mode() != crate::EntryMode::Tree {
                return Ok(None);
            }
            id = entry.id();
        }
        Ok(None)
    }

    fn write_ls_response(&mut self, resolved: Option<(u32, ObjectId)>, path: &[u8]) -> Result<()> {
        if let Some((mode, id)) = resolved {
            let kind = self
                .repository
                .read_object(id, self.options.max_object_size)?
                .kind();
            self.push_response(
                format!(
                    "{mode:06o} {} {id}\t{}\n",
                    kind_name(kind),
                    String::from_utf8_lossy(path)
                )
                .as_bytes(),
            )
        } else {
            self.push_response(format!("missing {}\n", String::from_utf8_lossy(path)).as_bytes())
        }
    }

    fn push_response(&mut self, data: &[u8]) -> Result<()> {
        let length = self
            .result
            .responses
            .len()
            .checked_add(data.len())
            .ok_or_else(|| Error::InvalidRepository("fast-import response overflow".into()))?;
        if length > self.options.max_response_size {
            return import_error("fast-import responses exceed limit");
        }
        self.result.responses.extend_from_slice(data);
        Ok(())
    }

    fn bump_command(&mut self) -> Result<()> {
        self.commands += 1;
        if self.commands > self.options.max_commands {
            return import_error("fast-import command count exceeds limit");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum FileChange {
    Modify {
        mode: u32,
        id: ObjectId,
        path: Vec<u8>,
    },
    Delete(Vec<u8>),
    Copy {
        source: Vec<u8>,
        destination: Vec<u8>,
    },
    Rename {
        source: Vec<u8>,
        destination: Vec<u8>,
    },
    DeleteAll,
}

struct StreamCursor<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> StreamCursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn peek_line(&self) -> Option<&'a [u8]> {
        line_at(self.data, self.position).map(|(line, _)| line)
    }

    fn take_line(&mut self) -> Option<&'a [u8]> {
        let (line, next) = line_at(self.data, self.position)?;
        self.position = next;
        Some(line)
    }

    fn take_bytes(&mut self, size: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(size)
            .ok_or_else(|| Error::InvalidRepository("fast-import data overflow".into()))?;
        let data = self
            .data
            .get(self.position..end)
            .ok_or_else(|| Error::InvalidRepository("truncated fast-import data".into()))?;
        self.position = end;
        if self.data.get(self.position) == Some(&b'\n') {
            self.position += 1;
        }
        Ok(data)
    }

    fn take_delimited(&mut self, delimiter: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        loop {
            let start = self.position;
            let line = self.take_line().ok_or_else(|| {
                Error::InvalidRepository("unterminated fast-import data delimiter".into())
            })?;
            if line == delimiter {
                return Ok(output);
            }
            output.extend_from_slice(&self.data[start..self.position]);
        }
    }
}

fn line_at(data: &[u8], start: usize) -> Option<(&[u8], usize)> {
    if start >= data.len() {
        return None;
    }
    let end = data[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(data.len(), |offset| start + offset);
    let next = end + usize::from(end < data.len());
    Some((&data[start..end], next))
}

fn is_file_command(line: &[u8]) -> bool {
    line == b"deleteall"
        || line.starts_with(b"M ")
        || line.starts_with(b"D ")
        || line.starts_with(b"C ")
        || line.starts_with(b"R ")
        || line.starts_with(b"N ")
}

fn parse_mark(value: &[u8]) -> Result<u64> {
    let digits = value
        .strip_prefix(b":")
        .ok_or_else(|| Error::InvalidRepository("mark must begin with colon".into()))?;
    let mark = parse_decimal(digits, "mark")? as u64;
    if mark == 0 {
        return import_error("mark zero is reserved");
    }
    Ok(mark)
}

fn parse_decimal(value: &[u8], label: &str) -> Result<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return import_error(format!("invalid fast-import {label}"));
    }
    parse_text(value, label)?
        .parse()
        .map_err(|_| Error::InvalidRepository(format!("fast-import {label} overflows")))
}

fn parse_ref(value: &[u8]) -> Result<String> {
    let value = parse_text(value, "reference")?.to_owned();
    ReferenceName::new(value.clone())?;
    Ok(value)
}

fn parse_text<'a>(value: &'a [u8], label: &str) -> Result<&'a str> {
    std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository(format!("non-UTF-8 fast-import {label}")))
}

fn parse_mode(value: &[u8]) -> Result<u32> {
    let mode = u32::from_str_radix(parse_text(value, "mode")?, 8)
        .map_err(|_| Error::InvalidRepository("invalid fast-import mode".into()))?;
    match mode {
        0o644 => Ok(0o100_644),
        0o755 => Ok(0o100_755),
        0o100_644 | 0o100_755 | 0o120_000 | 0o160_000 | 0o040_000 => Ok(mode),
        _ => import_error("unsupported fast-import mode"),
    }
}

fn mode_kind(mode: u32) -> ObjectKind {
    match mode {
        0o040_000 => ObjectKind::Tree,
        0o160_000 => ObjectKind::Commit,
        _ => ObjectKind::Blob,
    }
}

fn mode_number(mode: crate::EntryMode) -> u32 {
    match mode {
        crate::EntryMode::Blob => 0o100_644,
        crate::EntryMode::BlobExecutable => 0o100_755,
        crate::EntryMode::Link => 0o120_000,
        crate::EntryMode::Tree => 0o040_000,
        crate::EntryMode::Gitlink => 0o160_000,
    }
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn take_token<'a>(value: &'a [u8], label: &str) -> Result<(&'a [u8], &'a [u8])> {
    split_once(value, b' ', label)
}

fn split_once<'a>(value: &'a [u8], delimiter: u8, label: &str) -> Result<(&'a [u8], &'a [u8])> {
    let position = value
        .iter()
        .position(|byte| *byte == delimiter)
        .ok_or_else(|| Error::InvalidRepository(format!("fast-import {label} missing")))?;
    if position == 0 || position + 1 >= value.len() {
        return import_error(format!("fast-import {label} is empty"));
    }
    Ok((&value[..position], &value[position + 1..]))
}

fn parse_path(value: &[u8]) -> Result<Vec<u8>> {
    let path = if value.starts_with(b"\"") {
        let (path, consumed) = parse_quoted_path(value)?;
        if consumed != value.len() {
            return import_error("trailing bytes after quoted fast-import path");
        }
        path
    } else {
        value.to_vec()
    };
    validate_import_path(&path, true)?;
    Ok(path)
}

fn parse_two_paths(value: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (source, consumed) = if value.starts_with(b"\"") {
        parse_quoted_path(value)?
    } else {
        let position = value
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::InvalidRepository("filecopy paths missing".into()))?;
        (value[..position].to_vec(), position)
    };
    let rest = value
        .get(consumed..)
        .and_then(|value| value.strip_prefix(b" "))
        .ok_or_else(|| Error::InvalidRepository("filecopy destination missing".into()))?;
    let destination = parse_path(rest)?;
    validate_import_path(&source, false)?;
    Ok((source, destination))
}

fn parse_quoted_path(value: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut output = Vec::new();
    let mut cursor = 1usize;
    while cursor < value.len() {
        match value[cursor] {
            b'"' => return Ok((output, cursor + 1)),
            b'\\' => {
                cursor += 1;
                let escaped = *value.get(cursor).ok_or_else(|| {
                    Error::InvalidRepository("truncated quoted path escape".into())
                })?;
                if escaped.is_ascii_digit() && escaped < b'8' {
                    let digits = value.get(cursor..cursor + 3).ok_or_else(|| {
                        Error::InvalidRepository("truncated octal path escape".into())
                    })?;
                    if !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                        return import_error("invalid octal path escape");
                    }
                    output
                        .push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + digits[2] - b'0');
                    cursor += 2;
                } else {
                    output.push(match escaped {
                        b'a' => 7,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'v' => 11,
                        b'\\' => b'\\',
                        b'"' => b'"',
                        _ => return import_error("unknown quoted path escape"),
                    });
                }
            }
            byte => output.push(byte),
        }
        cursor += 1;
    }
    import_error("unterminated quoted fast-import path")
}

fn validate_import_path(path: &[u8], allow_empty: bool) -> Result<()> {
    if (!allow_empty && path.is_empty())
        || path.starts_with(b"/")
        || path.ends_with(b"/")
        || path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|part| matches!(part, b"." | b"..") || (!allow_empty && part.is_empty()))
        || (!path.is_empty() && path.split(|byte| *byte == b'/').any(<[u8]>::is_empty))
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(path).into_owned().into(),
        ));
    }
    Ok(())
}

fn delete_path(files: &mut BTreeMap<Vec<u8>, (u32, ObjectId)>, path: &[u8]) {
    files.retain(|candidate, _| !path_selected(candidate, path));
}

fn copy_path(
    files: &mut BTreeMap<Vec<u8>, (u32, ObjectId)>,
    source: &[u8],
    destination: &[u8],
    rename: bool,
) -> Result<()> {
    if source == destination || (rename && path_selected(destination, source)) {
        return import_error("invalid overlapping fast-import copy/rename");
    }
    let selected = files
        .iter()
        .filter(|(path, _)| path_selected(path, source))
        .map(|(path, value)| (path.clone(), *value))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return import_error("fast-import copy source does not exist");
    }
    delete_path(files, destination);
    if rename {
        delete_path(files, source);
    }
    for (path, value) in selected {
        let suffix = if path == source {
            &[][..]
        } else {
            &path[source.len() + 1..]
        };
        let target = join_import_path(destination, suffix)?;
        files.insert(target, value);
    }
    Ok(())
}

fn path_selected(candidate: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || candidate == prefix
        || (candidate.starts_with(prefix) && candidate.get(prefix.len()) == Some(&b'/'))
}

fn join_import_path(parent: &[u8], child: &[u8]) -> Result<Vec<u8>> {
    if parent.is_empty() {
        return Ok(child.to_vec());
    }
    if child.is_empty() {
        return Ok(parent.to_vec());
    }
    let mut output = Vec::with_capacity(
        parent
            .len()
            .checked_add(child.len() + 1)
            .ok_or_else(|| Error::InvalidRepository("fast-import path overflow".into()))?,
    );
    output.extend_from_slice(parent);
    output.push(b'/');
    output.extend_from_slice(child);
    Ok(output)
}

fn import_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(message.into()))
}

#[cfg(test)]
mod tests {
    use super::FastImportOptions;
    use crate::{InitOptions, MemoryFileSystem, ObjectKind, Repository};

    #[test]
    fn imports_marks_commits_inline_edits_tags_resets_aliases_and_queries() {
        let repository = repository();
        let stream = b"feature done\n\
blob\n\
mark :1\n\
data 5\nhello\n\
commit refs/heads/main\n\
mark :2\n\
committer Importer <i@example.com> 100 +0000\n\
data 4\none\n\
M 100644 :1 \"dir/a b\"\n\
commit refs/heads/main\n\
mark :3\n\
author Author <a@example.com> 101 +0130\n\
committer Importer <i@example.com> 102 +0000\n\
data <<MSG\nsecond\nMSG\n\
C \"dir/a b\" copied\n\
R copied moved\n\
M 755 inline script\n\
data 4\nrun\n\
D \"dir/a b\"\n\
ls \"moved\"\n\
tag v1\n\
mark :4\n\
from :3\n\
tagger Tagger <t@example.com> 103 -0200\n\
data 3\ntag\n\
alias\n\
mark :5\n\
to :3\n\
reset refs/heads/copy\n\
from :5\n\
get-mark :3\n\
cat-blob :1\n\
ls :3 moved\n\
progress imported\n\
done\n";
        let result = repository
            .fast_import(
                stream,
                &FastImportOptions {
                    require_done: true,
                    ..FastImportOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.blobs, 1);
        assert_eq!(result.commits, 2);
        assert_eq!(result.tags, 1);
        assert_eq!(result.marks().len(), 5);
        let tip = repository.resolve_reference("refs/heads/main").unwrap();
        assert_eq!(tip, result.marks()[&3]);
        assert_eq!(
            repository.resolve_reference("refs/heads/copy").unwrap(),
            tip
        );
        assert_eq!(
            repository.resolve_reference("refs/tags/v1").unwrap(),
            result.marks()[&4]
        );
        let commit = repository.read_commit(tip, 4096).unwrap();
        assert_eq!(commit.parents(), &[result.marks()[&2]]);
        assert_eq!(commit.author().name(), "Author");
        let leaves = repository.flattened_tree(commit.tree(), 4096).unwrap();
        assert_eq!(
            leaves
                .iter()
                .map(|entry| entry.path.as_slice())
                .collect::<Vec<_>>(),
            [b"moved".as_slice(), b"script".as_slice()]
        );
        assert_eq!(leaves[1].raw_mode, 0o100_755);
        let responses = String::from_utf8_lossy(result.responses());
        assert!(responses.contains(&format!("{tip}\n")));
        assert!(responses.contains(" blob 5\nhello\n"));
        assert!(responses.contains("100644 blob"));
        assert_eq!(responses.matches("100644 blob").count(), 2);
        assert!(responses.ends_with("progress imported\n"));
    }

    #[test]
    fn rejects_truncation_limits_non_fast_forwards_and_missing_done_without_ref_leaks() {
        let repository = repository();
        assert!(
            repository
                .fast_import(b"blob\ndata 9\nshort", &FastImportOptions::default())
                .is_err()
        );
        assert!(
            repository
                .fast_import(
                    b"blob\ndata 2\nok\n",
                    &FastImportOptions {
                        max_data_size: 1,
                        ..FastImportOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            repository
                .fast_import(
                    b"blob\nmark :1\ndata 1\nx\ncommit refs/heads/wrong\ncommitter A <a@b> 1 +0000\ndata 0\nfrom :1\ndone\n",
                    &FastImportOptions::default(),
                )
                .is_err()
        );
        assert!(repository.resolve_reference("refs/heads/wrong").is_err());
        assert!(
            repository
                .fast_import(
                    b"commit refs/heads/leak\ncommitter A <a@b> 1 +0000\ndata 0\n",
                    &FastImportOptions {
                        require_done: true,
                        ..FastImportOptions::default()
                    }
                )
                .is_err()
        );
        assert!(repository.resolve_reference("refs/heads/leak").is_err());

        repository
            .fast_import(
                b"commit refs/heads/main\nmark :1\ncommitter A <a@b> 1 +0000\ndata 0\ncommit refs/heads/main\ncommitter A <a@b> 2 +0000\ndata 0\ndone\n",
                &FastImportOptions::default(),
            )
            .unwrap();
        let old = repository
            .resolve_revision_id("main~1", &crate::RevisionOptions::default())
            .unwrap();
        let stream = format!("reset refs/heads/main\nfrom {old}\ndone\n");
        assert!(
            repository
                .fast_import(stream.as_bytes(), &FastImportOptions::default())
                .is_err()
        );
    }

    #[test]
    fn checkpoint_publishes_completed_prefix_before_later_parse_error() {
        let repository = repository();
        let stream =
            b"commit refs/heads/main\ncommitter A <a@b> 1 +0000\ndata 0\ncheckpoint\nunknown\n";
        assert!(
            repository
                .fast_import(stream, &FastImportOptions::default())
                .is_err()
        );
        let id = repository.resolve_reference("refs/heads/main").unwrap();
        assert_eq!(
            repository.read_object(id, 4096).unwrap().kind(),
            ObjectKind::Commit
        );
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }
}
