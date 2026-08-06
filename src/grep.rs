//! Bounded fixed-string search across tracked Git content.

use std::collections::BTreeSet;

use crate::{
    EntryMode, Error, LsFilesOptions, LsTreeOptions, ObjectId, ObjectKind, Repository, Result,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GrepTarget {
    Worktree,
    Index,
    Treeish(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrepBinaryMode {
    Default,
    Text,
    WithoutMatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct GrepOptions {
    pub target: GrepTarget,
    pub paths: Vec<Vec<u8>>,
    pub ignore_case: bool,
    pub invert_match: bool,
    pub word_regexp: bool,
    pub binary: GrepBinaryMode,
    pub max_files: usize,
    pub max_matches: usize,
    pub max_file_size: usize,
    pub max_depth: usize,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            target: GrepTarget::Worktree,
            paths: Vec::new(),
            ignore_case: false,
            invert_match: false,
            word_regexp: false,
            binary: GrepBinaryMode::Default,
            max_files: 10_000_000,
            max_matches: 10_000_000,
            max_file_size: 1024 * 1024 * 1024,
            max_depth: 4096,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepMatch {
    path: Vec<u8>,
    line_number: usize,
    column: usize,
    line: Vec<u8>,
    binary: bool,
}

impl GrepMatch {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// One-based line number, or zero for a binary-file match.
    #[must_use]
    pub const fn line_number(&self) -> usize {
        self.line_number
    }

    /// One-based byte column, or zero for an inverted or binary match.
    #[must_use]
    pub const fn column(&self) -> usize {
        self.column
    }

    /// Matching line without its line-feed terminator.
    #[must_use]
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    #[must_use]
    pub const fn is_binary(&self) -> bool {
        self.binary
    }
}

struct SearchFile {
    path: Vec<u8>,
    source: SearchSource,
}

enum SearchSource {
    Worktree,
    Object(ObjectId),
}

impl Repository {
    /// Search tracked files for any fixed byte string.
    ///
    /// The default target reads stage-zero, non-sparse tracked worktree files.
    /// `Index` searches their stored blobs, and `Treeish` recursively searches
    /// regular blobs from the named revision. Patterns are combined as in repeated
    /// `git grep -F -e` options.
    ///
    /// # Errors
    /// Returns an error for no patterns, unsafe paths, inaccessible worktree
    /// files, malformed index/tree/object data, wrong object types, exceeded
    /// resource limits, or storage failures.
    pub fn grep_fixed(
        &self,
        patterns: &[Vec<u8>],
        options: &GrepOptions,
    ) -> Result<Vec<GrepMatch>> {
        validate(patterns, &options.paths)?;
        let files = self.grep_files(options)?;
        if files.len() > options.max_files {
            return Err(Error::InvalidRepository("grep exceeds file limit".into()));
        }
        let worktree = match options.target {
            GrepTarget::Worktree => Some(self.work_tree().ok_or_else(|| {
                Error::InvalidRepository("worktree grep requires a non-bare repository".into())
            })?),
            GrepTarget::Index | GrepTarget::Treeish(_) => None,
        };
        let mut output = Vec::new();
        for file in files {
            let contents = match file.source {
                SearchSource::Worktree => {
                    let path = worktree
                        .ok_or_else(|| Error::InvalidRepository("worktree is unavailable".into()))?
                        .join(crate::status::worktree_path(&file.path)?);
                    let maximum = u64::try_from(options.max_file_size).unwrap_or(u64::MAX);
                    if self.filesystem().metadata(&path)?.len() > maximum {
                        return Err(Error::InvalidRepository(
                            "grep file exceeds size limit".into(),
                        ));
                    }
                    self.filesystem().read(&path)?
                }
                SearchSource::Object(id) => {
                    let object = self.read_object(id, options.max_file_size)?;
                    if object.kind() != ObjectKind::Blob {
                        return Err(Error::InvalidObject("grep source is not a blob".into()));
                    }
                    object.data().to_vec()
                }
            };
            if contents.len() > options.max_file_size {
                return Err(Error::InvalidRepository(
                    "grep file exceeds size limit".into(),
                ));
            }
            search_file(&file.path, &contents, patterns, options, &mut output)?;
        }
        Ok(output)
    }

    fn grep_files(&self, options: &GrepOptions) -> Result<Vec<SearchFile>> {
        match &options.target {
            GrepTarget::Worktree | GrepTarget::Index => {
                let entries = self.ls_files(&LsFilesOptions {
                    cached: true,
                    max_entries: options.max_files,
                    max_depth: options.max_depth,
                    max_object_size: options.max_file_size,
                    ..LsFilesOptions::default()
                })?;
                let mut seen = BTreeSet::new();
                let mut output = Vec::new();
                for entry in entries {
                    if entry.stage() != Some(0)
                        || entry.intent_to_add()
                        || !matches!(entry.mode(), Some(0o100_644 | 0o100_755))
                        || (matches!(options.target, GrepTarget::Worktree) && entry.skip_worktree())
                        || !selected(entry.path(), &options.paths)
                        || !seen.insert(entry.path().to_vec())
                    {
                        continue;
                    }
                    let source = match options.target {
                        GrepTarget::Index => SearchSource::Object(entry.id().ok_or_else(|| {
                            Error::InvalidRepository("cached grep entry has no object ID".into())
                        })?),
                        GrepTarget::Worktree if entry.assume_valid() => {
                            SearchSource::Object(entry.id().ok_or_else(|| {
                                Error::InvalidRepository("grep entry has no object ID".into())
                            })?)
                        }
                        GrepTarget::Worktree => SearchSource::Worktree,
                        GrepTarget::Treeish(_) => unreachable!(),
                    };
                    output.push(SearchFile {
                        path: entry.path().to_vec(),
                        source,
                    });
                }
                Ok(output)
            }
            GrepTarget::Treeish(treeish) => {
                let entries = self.ls_tree(
                    treeish,
                    &LsTreeOptions {
                        recursive: true,
                        paths: options.paths.clone(),
                        max_entries: options.max_files,
                        max_depth: options.max_depth,
                        revision: crate::RevisionOptions {
                            max_object_size: options.max_file_size,
                            ..crate::RevisionOptions::default()
                        },
                        ..LsTreeOptions::default()
                    },
                )?;
                Ok(entries
                    .into_iter()
                    .filter(|entry| {
                        matches!(entry.mode(), EntryMode::Blob | EntryMode::BlobExecutable)
                    })
                    .map(|entry| SearchFile {
                        path: entry.path().to_vec(),
                        source: SearchSource::Object(entry.id()),
                    })
                    .collect())
            }
        }
    }
}

fn search_file(
    path: &[u8],
    contents: &[u8],
    patterns: &[Vec<u8>],
    options: &GrepOptions,
    output: &mut Vec<GrepMatch>,
) -> Result<()> {
    let binary = contents.iter().take(8000).any(|byte| *byte == 0);
    if binary && options.binary == GrepBinaryMode::WithoutMatch {
        return Ok(());
    }
    if contents.is_empty() {
        return Ok(());
    }
    let mut lines = contents.split(|byte| *byte == b'\n').peekable();
    let mut line_number = 0;
    while let Some(raw_line) = lines.next() {
        if raw_line.is_empty() && lines.peek().is_none() && contents.ends_with(b"\n") {
            break;
        }
        line_number += 1;
        let position = patterns
            .iter()
            .filter_map(|pattern| {
                find_pattern(raw_line, pattern, options).map(|position| (position, pattern.len()))
            })
            .min_by_key(|(position, _)| *position);
        let matched = position.is_some();
        if matched == options.invert_match {
            continue;
        }
        if binary && options.binary == GrepBinaryMode::Default {
            output.push(GrepMatch {
                path: path.to_vec(),
                line_number: 0,
                column: 0,
                line: Vec::new(),
                binary: true,
            });
            enforce_match_limit(output, options.max_matches)?;
            break;
        }
        output.push(GrepMatch {
            path: path.to_vec(),
            line_number,
            column: position.map_or(0, |(position, _)| position + 1),
            line: raw_line.to_vec(),
            binary: false,
        });
        enforce_match_limit(output, options.max_matches)?;
    }
    Ok(())
}

fn find_pattern(line: &[u8], pattern: &[u8], options: &GrepOptions) -> Option<usize> {
    if pattern.is_empty() {
        return Some(0);
    }
    if pattern.len() > line.len() {
        return None;
    }
    (0..=line.len() - pattern.len()).find(|start| {
        let end = *start + pattern.len();
        bytes_equal(&line[*start..end], pattern, options.ignore_case)
            && (!options.word_regexp
                || ((*start == 0 || !is_word(line[*start - 1]))
                    && (end == line.len() || !is_word(line[end]))))
    })
}

fn bytes_equal(left: &[u8], right: &[u8], ignore_case: bool) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left == right || (ignore_case && left.eq_ignore_ascii_case(right)))
}

const fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn enforce_match_limit(output: &[GrepMatch], limit: usize) -> Result<()> {
    if output.len() > limit {
        Err(Error::InvalidRepository("grep exceeds match limit".into()))
    } else {
        Ok(())
    }
}

fn selected(path: &[u8], specs: &[Vec<u8>]) -> bool {
    specs.is_empty()
        || specs.iter().any(|spec| {
            path == spec
                || (path.starts_with(spec)
                    && path.len() > spec.len()
                    && path.get(spec.len()) == Some(&b'/'))
        })
}

fn validate(patterns: &[Vec<u8>], paths: &[Vec<u8>]) -> Result<()> {
    if patterns.is_empty() {
        return Err(Error::InvalidRepository("grep requires a pattern".into()));
    }
    for path in paths {
        if path.is_empty()
            || path.starts_with(b"/")
            || path.ends_with(b"/")
            || path.contains(&0)
            || path
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || matches!(component, b"." | b".."))
        {
            return Err(Error::InvalidPath(
                String::from_utf8_lossy(path).into_owned().into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        FileSystem, Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, StatData, Tree,
        TreeEntry,
    };

    #[test]
    fn searches_worktree_index_and_binary_content() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let stored = repository
            .write_object(ObjectKind::Blob, b"Alpha old\nsecond\n")
            .unwrap();
        let binary = repository
            .write_object(ObjectKind::Blob, b"hit\0binary")
            .unwrap();
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::new(b"file".to_vec(), 0o100_644, stored, StatData::default())
                            .unwrap(),
                        IndexEntry::new(b"binary".to_vec(), 0o100_644, binary, StatData::default())
                            .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        fs.write(Path::new("repo/file"), b"Alpha new\nsecond\n")
            .unwrap();
        fs.write(Path::new("repo/binary"), b"hit\0binary").unwrap();

        let worktree = repository
            .grep_fixed(&[b"new".to_vec()], &GrepOptions::default())
            .unwrap();
        assert_eq!(worktree[0].line(), b"Alpha new");
        assert_eq!(worktree[0].line_number(), 1);
        assert_eq!(worktree[0].column(), 7);
        let cached = repository
            .grep_fixed(
                &[b"old".to_vec()],
                &GrepOptions {
                    target: GrepTarget::Index,
                    ..GrepOptions::default()
                },
            )
            .unwrap();
        assert_eq!(cached[0].path(), b"file");
        let binary = repository
            .grep_fixed(
                &[b"hit".to_vec()],
                &GrepOptions {
                    target: GrepTarget::Index,
                    ..GrepOptions::default()
                },
            )
            .unwrap();
        assert!(binary[0].is_binary());
    }

    #[test]
    fn supports_case_word_line_inversion_and_limits() {
        let options = GrepOptions {
            ignore_case: true,
            word_regexp: true,
            ..GrepOptions::default()
        };
        assert_eq!(find_pattern(b"one FOO two", b"foo", &options), Some(4));
        assert_eq!(find_pattern(b"food", b"foo", &options), None);
        let mut output = Vec::new();
        search_file(
            b"file",
            b"hit\n\nmiss\n",
            &[b"hit".to_vec()],
            &GrepOptions {
                invert_match: true,
                ..GrepOptions::default()
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(
            output
                .iter()
                .map(GrepMatch::line_number)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

        let error = search_file(
            b"file",
            b"hit\n",
            &[b"hit".to_vec()],
            &GrepOptions {
                max_matches: 0,
                ..GrepOptions::default()
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidRepository(_)));
    }

    #[test]
    fn searches_treeish_recursively_with_path_selection() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let selected = repository
            .write_object(ObjectKind::Blob, b"needle selected\n")
            .unwrap();
        let omitted = repository
            .write_object(ObjectKind::Blob, b"needle omitted\n")
            .unwrap();
        let nested = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"selected".to_vec(), selected).unwrap(),
                    TreeEntry::new(EntryMode::Blob, b"omitted".to_vec(), omitted).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let root = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"dir".to_vec(), nested).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let matches = repository
            .grep_fixed(
                &[b"needle".to_vec()],
                &GrepOptions {
                    target: GrepTarget::Treeish(root.to_string()),
                    paths: vec![b"dir/selected".to_vec()],
                    ..GrepOptions::default()
                },
            )
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path(), b"dir/selected");
    }
}
