//! Inspect loose or packed objects and serve the core cat-file batch protocol.

use std::fmt::Write as _;

use crate::{
    EntryMode, Error, Object, ObjectId, ObjectKind, Repository, Result, RevisionOptions, Tree,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatFileMode {
    Content,
    Pretty,
    Type,
    Size,
    Exists,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatFileResult {
    pub id: ObjectId,
    pub exists: bool,
    pub kind: Option<ObjectKind>,
    pub size: Option<usize>,
    pub output: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatFileBatchOptions {
    /// Include object contents after each information header.
    pub contents: bool,
    /// Use NUL for input and output record termination, corresponding to `-Z`.
    pub nul_terminated: bool,
    pub max_input_bytes: usize,
    pub max_requests: usize,
    pub max_object_size: usize,
    pub max_output_bytes: usize,
}

impl Default for CatFileBatchOptions {
    fn default() -> Self {
        Self {
            contents: true,
            nul_terminated: false,
            max_input_bytes: 1024 * 1024 * 1024,
            max_requests: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
            max_output_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Inspect one object ID, optionally peeling it to an expected type.
    ///
    /// # Errors
    /// Returns an error for a missing, corrupt, oversized, or unpeelable
    /// object, or when pretty tree formatting exceeds the output limit.
    pub fn cat_file(
        &self,
        id: ObjectId,
        mode: CatFileMode,
        expected: Option<ObjectKind>,
        max_object_size: usize,
        max_output_size: usize,
    ) -> Result<CatFileResult> {
        if mode == CatFileMode::Exists {
            let exists = self.contains_object(id)?;
            return Ok(CatFileResult {
                id,
                exists,
                kind: None,
                size: None,
                output: Vec::new(),
            });
        }
        let (resolved, object) = self.cat_file_object(id, expected, max_object_size)?;
        let size = object.data().len();
        let output = match mode {
            CatFileMode::Pretty if object.kind() == ObjectKind::Tree => {
                pretty_tree(&Tree::parse(object.data())?, max_output_size)?
            }
            CatFileMode::Content | CatFileMode::Pretty => object.data().to_vec(),
            CatFileMode::Type => {
                let mut output = object.kind().as_bytes().to_vec();
                output.push(b'\n');
                output
            }
            CatFileMode::Size => format!("{size}\n").into_bytes(),
            CatFileMode::Exists => unreachable!(),
        };
        if output.len() > max_output_size {
            return cat_error("cat-file output exceeds limit");
        }
        Ok(CatFileResult {
            id: resolved,
            exists: true,
            kind: Some(object.kind()),
            size: Some(size),
            output,
        })
    }

    /// Process the default `--batch` or `--batch-check` byte protocol.
    ///
    /// Each request is a revision expression terminated by LF, or by NUL when
    /// configured. Unresolvable requests produce `<request> missing` records.
    ///
    /// # Errors
    /// Returns an error for exceeded bounds, non-UTF-8 revision expressions,
    /// corrupt objects, or storage failures.
    pub fn cat_file_batch(&self, input: &[u8], options: &CatFileBatchOptions) -> Result<Vec<u8>> {
        if input.len() > options.max_input_bytes {
            return cat_error("cat-file batch input exceeds limit");
        }
        let delimiter = if options.nul_terminated { 0 } else { b'\n' };
        let mut output = Vec::new();
        let mut requests = 0;
        for record in input.split(|byte| *byte == delimiter) {
            let expression = record;
            if expression.is_empty() {
                continue;
            }
            if requests == options.max_requests {
                return cat_error("cat-file batch request count exceeds limit");
            }
            requests += 1;
            let text = std::str::from_utf8(expression)
                .map_err(|_| Error::InvalidRevision("batch expression is not UTF-8".into()))?;
            let revision_options = RevisionOptions {
                max_object_size: options.max_object_size,
                ..RevisionOptions::default()
            };
            match self.resolve_revision_id(text, &revision_options) {
                Ok(id) => {
                    let object = self.read_object(id, options.max_object_size)?;
                    append_bounded(
                        &mut output,
                        format!("{id} {} {}", kind_name(object.kind()), object.data().len())
                            .as_bytes(),
                        options.max_output_bytes,
                    )?;
                    append_byte(&mut output, delimiter, options.max_output_bytes)?;
                    if options.contents {
                        append_bounded(&mut output, object.data(), options.max_output_bytes)?;
                        append_byte(&mut output, delimiter, options.max_output_bytes)?;
                    }
                }
                Err(
                    Error::NotFound(_)
                    | Error::InvalidRevision(_)
                    | Error::InvalidReferenceName(_)
                    | Error::InvalidObjectId(_)
                    | Error::AmbiguousRevision(_),
                ) => {
                    append_bounded(&mut output, expression, options.max_output_bytes)?;
                    append_bounded(&mut output, b" missing", options.max_output_bytes)?;
                    append_byte(&mut output, delimiter, options.max_output_bytes)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(output)
    }

    fn cat_file_object(
        &self,
        id: ObjectId,
        expected: Option<ObjectKind>,
        max_size: usize,
    ) -> Result<(ObjectId, Object)> {
        let mut current = id;
        for _ in 0..=64 {
            let object = self.read_object(current, max_size)?;
            let Some(expected) = expected else {
                return Ok((current, object));
            };
            if object.kind() == expected {
                return Ok((current, object));
            }
            current = match (object.kind(), expected) {
                (ObjectKind::Tag, _) => crate::AnnotatedTag::parse(object.data())?.target(),
                (ObjectKind::Commit, ObjectKind::Tree) => {
                    crate::Commit::parse(object.data())?.tree()
                }
                _ => return cat_error("object cannot be peeled to requested type"),
            };
        }
        cat_error("cat-file peel depth exceeds limit")
    }
}

fn pretty_tree(tree: &Tree, max_output: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for entry in tree.entries() {
        let mode = match entry.mode() {
            EntryMode::Tree => "040000",
            EntryMode::Blob => "100644",
            EntryMode::BlobExecutable => "100755",
            EntryMode::Link => "120000",
            EntryMode::Gitlink => "160000",
        };
        let mut header = String::new();
        write!(
            header,
            "{mode} {} {}\t",
            kind_name(entry.mode().object_kind()),
            entry.id()
        )
        .expect("writing to String cannot fail");
        append_bounded(&mut output, header.as_bytes(), max_output)?;
        quote_tree_name(&mut output, entry.name(), max_output)?;
        append_byte(&mut output, b'\n', max_output)?;
    }
    Ok(output)
}

fn quote_tree_name(output: &mut Vec<u8>, name: &[u8], max_output: usize) -> Result<()> {
    let quoted = name
        .iter()
        .any(|byte| !matches!(*byte, b' '..=b'~') || matches!(*byte, b'"' | b'\\'));
    if !quoted {
        return append_bounded(output, name, max_output);
    }
    append_byte(output, b'"', max_output)?;
    for byte in name {
        match *byte {
            b'\n' => append_bounded(output, b"\\n", max_output)?,
            b'\t' => append_bounded(output, b"\\t", max_output)?,
            b'\r' => append_bounded(output, b"\\r", max_output)?,
            b'\x08' => append_bounded(output, b"\\b", max_output)?,
            b'\x0c' => append_bounded(output, b"\\f", max_output)?,
            b'\x0b' => append_bounded(output, b"\\v", max_output)?,
            b'\x07' => append_bounded(output, b"\\a", max_output)?,
            b'"' | b'\\' => {
                append_byte(output, b'\\', max_output)?;
                append_byte(output, *byte, max_output)?;
            }
            b' '..=b'~' => append_byte(output, *byte, max_output)?,
            value => append_bounded(output, format!("\\{value:03o}").as_bytes(), max_output)?,
        }
    }
    append_byte(output, b'"', max_output)
}

fn append_bounded(output: &mut Vec<u8>, value: &[u8], maximum: usize) -> Result<()> {
    if value.len() > maximum.saturating_sub(output.len()) {
        return cat_error("cat-file output exceeds limit");
    }
    output.extend_from_slice(value);
    Ok(())
}

fn append_byte(output: &mut Vec<u8>, value: u8, maximum: usize) -> Result<()> {
    if output.len() == maximum {
        return cat_error("cat-file output exceeds limit");
    }
    output.push(value);
    Ok(())
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn cat_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidObject(message.into()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CatFileBatchOptions, CatFileMode};
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, ObjectKind,
        PackOptions, Repository, Signature, TagBuilder, Tree, TreeEntry,
    };

    #[test]
    fn reports_content_type_size_exists_and_pretty_tree() {
        let repository = repository();
        let blob = repository
            .write_object(ObjectKind::Blob, b"hello\n")
            .unwrap();
        assert_eq!(
            repository
                .cat_file(blob, CatFileMode::Content, None, 1024, 1024)
                .unwrap()
                .output,
            b"hello\n"
        );
        assert_eq!(
            repository
                .cat_file(blob, CatFileMode::Type, None, 1024, 1024)
                .unwrap()
                .output,
            b"blob\n"
        );
        assert_eq!(
            repository
                .cat_file(blob, CatFileMode::Size, None, 1024, 1024)
                .unwrap()
                .output,
            b"6\n"
        );
        assert!(
            repository
                .cat_file(blob, CatFileMode::Exists, None, 1024, 1024)
                .unwrap()
                .exists
        );
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"a\tname".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let pretty = repository
            .cat_file(tree, CatFileMode::Pretty, None, 1024, 1024)
            .unwrap()
            .output;
        assert_eq!(
            pretty,
            format!("100644 blob {blob}\t\"a\\tname\"\n").as_bytes()
        );
    }

    #[test]
    fn peels_commit_and_tag_to_requested_types() {
        let repository = repository();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let signature = Signature::new("A", "a@example.com", 1, 0).unwrap();
        let commit = repository
            .write_commit(&CommitBuilder::new(tree, signature.clone(), signature.clone()).build())
            .unwrap();
        let tag = repository
            .write_tag(
                &TagBuilder::new(commit, ObjectKind::Commit, b"v1", signature)
                    .unwrap()
                    .build(),
                4096,
            )
            .unwrap();
        assert_eq!(
            repository
                .cat_file(
                    tag,
                    CatFileMode::Content,
                    Some(ObjectKind::Tree),
                    4096,
                    4096
                )
                .unwrap()
                .id,
            tree
        );
    }

    #[test]
    fn serves_bounded_newline_and_nul_batch_protocols() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"data").unwrap();
        let input = format!("{blob}\nmissing\n");
        assert_eq!(
            repository
                .cat_file_batch(input.as_bytes(), &CatFileBatchOptions::default())
                .unwrap(),
            format!("{blob} blob 4\ndata\nmissing missing\n").as_bytes()
        );
        assert_eq!(
            repository
                .cat_file_batch(b"missing rest here\n", &CatFileBatchOptions::default())
                .unwrap(),
            b"missing rest here missing\n"
        );
        assert_eq!(
            repository
                .cat_file_batch(
                    format!("{blob}\0").as_bytes(),
                    &CatFileBatchOptions {
                        contents: false,
                        nul_terminated: true,
                        ..CatFileBatchOptions::default()
                    }
                )
                .unwrap(),
            format!("{blob} blob 4\0").as_bytes()
        );
        assert!(
            repository
                .cat_file_batch(
                    format!("{blob}\n").as_bytes(),
                    &CatFileBatchOptions {
                        max_output_bytes: 3,
                        ..CatFileBatchOptions::default()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn reads_packed_only_objects_through_the_same_batch_path() {
        let filesystem = MemoryFileSystem::new();
        let repository = Repository::init(
            filesystem.clone(),
            ".",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"packed")
            .unwrap();
        repository
            .write_pack(&[blob], &PackOptions::default())
            .unwrap();
        let hex = blob.to_string();
        filesystem
            .remove_file(&Path::new("objects").join(&hex[..2]).join(&hex[2..]))
            .unwrap();
        assert_eq!(
            repository
                .cat_file_batch(
                    format!("{blob}\n").as_bytes(),
                    &CatFileBatchOptions::default()
                )
                .unwrap(),
            format!("{blob} blob 6\npacked\n").as_bytes()
        );
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
