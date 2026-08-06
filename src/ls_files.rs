//! Git index and worktree inventory.

use std::collections::BTreeSet;
use std::path::{Component, Path};

#[cfg(not(unix))]
use std::path::PathBuf;

use crate::{EntryMode, Error, IgnoreMatcher, IndexEntry, ObjectId, Repository, Result, StatData};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsFilesOptions {
    pub cached: bool,
    pub stage: bool,
    pub unmerged: bool,
    pub deleted: bool,
    pub modified: bool,
    pub others: bool,
    pub ignored: bool,
    pub exclude_standard: bool,
    pub killed: bool,
    pub deduplicate: bool,
    pub show_sparse_directories: bool,
    pub error_unmatch: bool,
    pub paths: Vec<Vec<u8>>,
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_object_size: usize,
}

impl Default for LsFilesOptions {
    fn default() -> Self {
        Self {
            cached: false,
            stage: false,
            unmerged: false,
            deleted: false,
            modified: false,
            others: false,
            ignored: false,
            exclude_standard: false,
            killed: false,
            deduplicate: false,
            show_sparse_directories: false,
            error_unmatch: false,
            paths: Vec::new(),
            max_entries: 10_000_000,
            max_depth: 4096,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LsFilesKind {
    Cached,
    Unmerged,
    Deleted,
    Modified,
    Other,
    Ignored,
    Killed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsFilesEntry {
    path: Vec<u8>,
    kind: LsFilesKind,
    mode: Option<u32>,
    id: Option<ObjectId>,
    stage: Option<u8>,
    assume_valid: bool,
    intent_to_add: bool,
    skip_worktree: bool,
}

impl LsFilesEntry {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
    #[must_use]
    pub const fn kind(&self) -> LsFilesKind {
        self.kind
    }
    #[must_use]
    pub const fn mode(&self) -> Option<u32> {
        self.mode
    }
    #[must_use]
    pub const fn id(&self) -> Option<ObjectId> {
        self.id
    }
    #[must_use]
    pub const fn stage(&self) -> Option<u8> {
        self.stage
    }
    #[must_use]
    pub const fn assume_valid(&self) -> bool {
        self.assume_valid
    }
    #[must_use]
    pub const fn intent_to_add(&self) -> bool {
        self.intent_to_add
    }
    #[must_use]
    pub const fn skip_worktree(&self) -> bool {
        self.skip_worktree
    }
}

impl Repository {
    /// List selected index and worktree classifications in Git's output order.
    ///
    /// With no selection flags, cached entries are returned. `unmerged` implies
    /// stage output and filters to stages 1–3. Other/killed worktree entries
    /// precede index classifications; index duplicates are retained unless
    /// `deduplicate` is requested.
    ///
    /// # Errors
    /// Returns an error for malformed index/tree/ignore data, bare worktree
    /// modes, inaccessible paths, unmatched required pathspecs, exceeded
    /// resource limits, or storage failures.
    #[allow(clippy::too_many_lines)]
    pub fn ls_files(&self, options: &LsFilesOptions) -> Result<Vec<LsFilesEntry>> {
        validate_specs(&options.paths)?;
        if options.ignored && (!options.exclude_standard || (!options.others && !options.cached)) {
            return Err(Error::InvalidRepository(
                "ignored ls-files mode requires cached or others and an exclusion source".into(),
            ));
        }
        let mut cached = options.cached;
        let mut stage = options.stage;
        if options.unmerged {
            stage = true;
        }
        if !(cached
            || stage
            || options.deleted
            || options.modified
            || options.others
            || options.killed)
        {
            cached = true;
        }
        let index = self.read_index()?;
        let entries = self.expanded_ls_files_entries(index.entries(), options)?;
        let tracked = entries
            .iter()
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let stage_zero = entries
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let needs_worktree = options.deleted
            || options.modified
            || options.others
            || options.killed
            || options.ignored;
        let work_tree = needs_worktree
            .then(|| {
                self.work_tree().ok_or_else(|| {
                    Error::InvalidRepository("ls-files mode requires a worktree".into())
                })
            })
            .transpose()?;
        let mut worktree_entries = Vec::new();
        if options.others || options.killed {
            let root = work_tree.ok_or_else(|| {
                Error::InvalidRepository("ls-files mode requires a worktree".into())
            })?;
            let mut matcher = if options.exclude_standard {
                self.ignore_matcher()?
            } else {
                IgnoreMatcher::default()
            };
            self.collect_ls_files_worktree(
                root,
                Path::new(""),
                0,
                options,
                &mut matcher,
                &mut worktree_entries,
            )?;
        }

        let mut output = Vec::new();
        if options.others {
            for item in &worktree_entries {
                if tracked.contains(&item.path) || !matches_specs(&item.path, &options.paths) {
                    continue;
                }
                if item.ignored == options.ignored {
                    output.push(worktree_result(
                        &item.path,
                        if item.ignored {
                            LsFilesKind::Ignored
                        } else {
                            LsFilesKind::Other
                        },
                    ));
                }
            }
        }
        if options.killed {
            for item in &worktree_entries {
                if tracked.contains(&item.path) || !matches_specs(&item.path, &options.paths) {
                    continue;
                }
                if is_killed(&item.path, &stage_zero) {
                    output.push(worktree_result(&item.path, LsFilesKind::Killed));
                }
            }
        }

        let mut matcher = options.ignored.then(|| self.ignore_matcher()).transpose()?;
        for entry in &entries {
            if !matches_specs(entry.path(), &options.paths) {
                continue;
            }
            if let Some(matcher) = &mut matcher {
                let root = work_tree.ok_or_else(|| {
                    Error::InvalidRepository("ignored ls-files mode requires a worktree".into())
                })?;
                add_parent_ignore_patterns(self, matcher, root, entry.path())?;
                if !matcher.is_ignored(entry.path(), entry.mode() == 0o040_000) {
                    continue;
                }
            }
            if (cached || stage) && (!options.unmerged || entry.stage() != 0) {
                output.push(index_result(
                    entry,
                    if entry.stage() == 0 {
                        LsFilesKind::Cached
                    } else {
                        LsFilesKind::Unmerged
                    },
                ));
            }
            if !(options.deleted || options.modified) || entry.skip_worktree() {
                continue;
            }
            let root = work_tree.ok_or_else(|| {
                Error::InvalidRepository("ls-files mode requires a worktree".into())
            })?;
            let full = root.join(crate::status::worktree_path(entry.path())?);
            let (missing, change) = match self.filesystem().metadata(&full) {
                Err(Error::NotFound(_)) => (true, None),
                Err(error) => return Err(error),
                Ok(metadata) => (
                    false,
                    crate::status::worktree_change(self, entry, &full, metadata)?,
                ),
            };
            if missing {
                if options.deleted {
                    output.push(index_result(entry, LsFilesKind::Deleted));
                }
                if options.modified {
                    output.push(index_result(entry, LsFilesKind::Modified));
                }
            } else if change.is_some() && options.modified {
                output.push(index_result(entry, LsFilesKind::Modified));
            }
        }
        if options.deduplicate {
            let mut seen = BTreeSet::new();
            output.retain(|entry| seen.insert(entry.path.clone()));
        }
        if options.error_unmatch {
            for spec in &options.paths {
                if !output
                    .iter()
                    .any(|entry| entry.path == *spec || path_below(&entry.path, spec))
                {
                    return Err(Error::NotFound(
                        String::from_utf8_lossy(spec).into_owned().into(),
                    ));
                }
            }
        }
        Ok(output)
    }

    fn expanded_ls_files_entries(
        &self,
        entries: &[IndexEntry],
        options: &LsFilesOptions,
    ) -> Result<Vec<IndexEntry>> {
        let mut output = Vec::new();
        for entry in entries {
            if entry.mode() == 0o040_000 && !options.show_sparse_directories {
                self.expand_sparse_entry(entry, 0, options, &mut output)?;
            } else {
                output.push(entry.clone());
            }
            if output.len() > options.max_entries {
                return Err(Error::InvalidRepository(
                    "ls-files exceeds entry limit".into(),
                ));
            }
        }
        Ok(output)
    }

    fn expand_sparse_entry(
        &self,
        entry: &IndexEntry,
        depth: usize,
        options: &LsFilesOptions,
        output: &mut Vec<IndexEntry>,
    ) -> Result<()> {
        if depth > options.max_depth {
            return Err(Error::InvalidRepository(
                "sparse index expansion exceeds depth limit".into(),
            ));
        }
        for child in self
            .read_tree(entry.id(), options.max_object_size)?
            .entries()
        {
            let mut path = entry.path().to_vec();
            path.push(b'/');
            path.extend_from_slice(child.name());
            if child.mode() == EntryMode::Tree {
                let directory = IndexEntry::new(path, 0o040_000, child.id(), StatData::default())?
                    .with_skip_worktree(true);
                self.expand_sparse_entry(&directory, depth + 1, options, output)?;
            } else {
                output.push(
                    IndexEntry::new(
                        path,
                        mode_number(child.mode()),
                        child.id(),
                        StatData::default(),
                    )?
                    .with_skip_worktree(true),
                );
            }
            if output.len() > options.max_entries {
                return Err(Error::InvalidRepository(
                    "ls-files exceeds entry limit".into(),
                ));
            }
        }
        Ok(())
    }

    fn collect_ls_files_worktree(
        &self,
        root: &Path,
        relative: &Path,
        depth: usize,
        options: &LsFilesOptions,
        matcher: &mut IgnoreMatcher,
        output: &mut Vec<WorktreeItem>,
    ) -> Result<()> {
        if depth > options.max_depth {
            return Err(Error::InvalidRepository(
                "ls-files worktree scan exceeds depth limit".into(),
            ));
        }
        let base = path_to_index(relative)?;
        if options.exclude_standard {
            matcher.add_worktree_patterns(self, root, &base)?;
        }
        for child in self.filesystem().read_dir(&root.join(relative))? {
            let child_relative = relative.join(child);
            if child_relative == Path::new(".git") || root.join(&child_relative) == self.git_dir() {
                continue;
            }
            let path = path_to_index(&child_relative)?;
            let metadata = self.filesystem().metadata(&root.join(&child_relative))?;
            if metadata.is_dir() {
                self.collect_ls_files_worktree(
                    root,
                    &child_relative,
                    depth + 1,
                    options,
                    matcher,
                    output,
                )?;
            } else {
                output.push(WorktreeItem {
                    ignored: matcher.is_ignored(&path, false),
                    path,
                });
                if output.len() > options.max_entries {
                    return Err(Error::InvalidRepository(
                        "ls-files exceeds entry limit".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

struct WorktreeItem {
    path: Vec<u8>,
    ignored: bool,
}

fn index_result(entry: &IndexEntry, kind: LsFilesKind) -> LsFilesEntry {
    LsFilesEntry {
        path: entry.path().to_vec(),
        kind,
        mode: Some(entry.mode()),
        id: Some(entry.id()),
        stage: Some(entry.stage()),
        assume_valid: entry.assume_valid(),
        intent_to_add: entry.intent_to_add(),
        skip_worktree: entry.skip_worktree(),
    }
}

fn worktree_result(path: &[u8], kind: LsFilesKind) -> LsFilesEntry {
    LsFilesEntry {
        path: path.to_vec(),
        kind,
        mode: None,
        id: None,
        stage: None,
        assume_valid: false,
        intent_to_add: false,
        skip_worktree: false,
    }
}

fn matches_specs(path: &[u8], specs: &[Vec<u8>]) -> bool {
    specs.is_empty()
        || specs
            .iter()
            .any(|spec| path == spec || path_below(path, spec))
}

fn path_below(path: &[u8], prefix: &[u8]) -> bool {
    path.len() > prefix.len() && path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/')
}

fn is_killed(path: &[u8], tracked: &BTreeSet<Vec<u8>>) -> bool {
    tracked
        .iter()
        .any(|candidate| path_below(path, candidate) || path_below(candidate, path))
}

fn validate_specs(specs: &[Vec<u8>]) -> Result<()> {
    for spec in specs {
        if spec.is_empty() || spec.starts_with(b"/") || spec.contains(&0) {
            return Err(Error::InvalidPath(
                String::from_utf8_lossy(spec).into_owned().into(),
            ));
        }
    }
    Ok(())
}

fn add_parent_ignore_patterns(
    repository: &Repository,
    matcher: &mut IgnoreMatcher,
    root: &Path,
    path: &[u8],
) -> Result<()> {
    let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    let mut base = Vec::new();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !base.is_empty() {
            base.push(b'/');
        }
        base.extend_from_slice(component);
        matcher.add_worktree_patterns(repository, root, &base)?;
    }
    Ok(())
}

const fn mode_number(mode: EntryMode) -> u32 {
    match mode {
        EntryMode::Blob => 0o100_644,
        EntryMode::BlobExecutable => 0o100_755,
        EntryMode::Link => 0o120_000,
        EntryMode::Tree => 0o040_000,
        EntryMode::Gitlink => 0o160_000,
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn path_to_index(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let mut output = Vec::new();
    for component in path.components() {
        if let Component::Normal(value) = component {
            if !output.is_empty() {
                output.push(b'/');
            }
            output.extend_from_slice(value.as_bytes());
        }
    }
    Ok(output)
}

#[cfg(not(unix))]
fn path_to_index(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/").into_bytes())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem, ObjectKind, Tree, TreeEntry};

    #[test]
    fn lists_cached_stages_deleted_modified_other_ignored_and_killed() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let old = repository.write_object(ObjectKind::Blob, b"old").unwrap();
        let conflict = repository
            .write_object(ObjectKind::Blob, b"conflict")
            .unwrap();
        let index = crate::Index::new(
            crate::IndexVersion::V2,
            vec![
                IndexEntry::new(b"deleted".to_vec(), 0o100_644, old, StatData::default()).unwrap(),
                IndexEntry::new(b"missing".to_vec(), 0o100_644, old, StatData::default()).unwrap(),
                IndexEntry::new(b"dir/file".to_vec(), 0o100_644, old, StatData::default()).unwrap(),
                IndexEntry::new(b"modified".to_vec(), 0o100_644, old, StatData::default()).unwrap(),
                IndexEntry::with_stage(
                    b"unmerged".to_vec(),
                    0o100_644,
                    conflict,
                    StatData::default(),
                    1,
                )
                .unwrap(),
                IndexEntry::with_stage(
                    b"unmerged".to_vec(),
                    0o100_644,
                    old,
                    StatData::default(),
                    2,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        repository.write_index(&index).unwrap();
        fs.create_dir_all(Path::new("repo/dir")).unwrap();
        fs.write(Path::new("repo/modified"), b"new").unwrap();
        fs.write(Path::new("repo/other"), b"other").unwrap();
        fs.write(Path::new("repo/.gitignore"), b"ignored\n")
            .unwrap();
        fs.write(Path::new("repo/ignored"), b"ignored").unwrap();
        fs.create_dir_all(Path::new("repo/deleted")).unwrap();
        fs.write(Path::new("repo/deleted/child"), b"killed")
            .unwrap();

        let result = repository
            .ls_files(&LsFilesOptions {
                cached: true,
                stage: true,
                deleted: true,
                modified: true,
                others: true,
                killed: true,
                ..LsFilesOptions::default()
            })
            .unwrap();
        assert!(
            result
                .iter()
                .any(|entry| entry.kind() == LsFilesKind::Other && entry.path() == b"other")
        );
        assert!(
            result.iter().any(
                |entry| entry.kind() == LsFilesKind::Killed && entry.path() == b"deleted/child"
            )
        );
        assert!(
            result
                .iter()
                .any(|entry| entry.kind() == LsFilesKind::Deleted && entry.path() == b"missing")
        );
        assert!(
            result
                .iter()
                .any(|entry| entry.kind() == LsFilesKind::Modified && entry.path() == b"deleted")
        );
        assert!(
            result
                .iter()
                .any(|entry| entry.kind() == LsFilesKind::Modified && entry.path() == b"modified")
        );
        assert_eq!(result.iter().filter(|entry| entry.path() == b"unmerged" && entry.kind() == LsFilesKind::Unmerged).count(), 2);
        let ignored = repository
            .ls_files(&LsFilesOptions {
                others: true,
                ignored: true,
                exclude_standard: true,
                ..LsFilesOptions::default()
            })
            .unwrap();
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].path(), b"ignored");

        let missing = result
            .iter()
            .filter(|entry| entry.path() == b"missing")
            .map(LsFilesEntry::kind)
            .collect::<Vec<_>>();
        assert!(missing.contains(&LsFilesKind::Deleted));
        assert!(missing.contains(&LsFilesKind::Modified));
    }

    #[test]
    fn defaults_to_cached_and_preserves_conflict_stages() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"value").unwrap();
        repository
            .write_index(
                &crate::Index::new(
                    crate::IndexVersion::V2,
                    vec![
                        IndexEntry::with_stage(
                            b"conflict".to_vec(),
                            0o100_644,
                            blob,
                            StatData::default(),
                            1,
                        )
                        .unwrap(),
                        IndexEntry::with_stage(
                            b"conflict".to_vec(),
                            0o100_644,
                            blob,
                            StatData::default(),
                            2,
                        )
                        .unwrap(),
                        IndexEntry::new(b"normal".to_vec(), 0o100_644, blob, StatData::default())
                            .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();

        let entries = repository.ls_files(&LsFilesOptions::default()).unwrap();
        assert_eq!(
            entries.iter().map(LsFilesEntry::path).collect::<Vec<_>>(),
            vec![
                b"conflict".as_slice(),
                b"conflict".as_slice(),
                b"normal".as_slice()
            ]
        );
        let unmerged = repository
            .ls_files(&LsFilesOptions {
                unmerged: true,
                ..LsFilesOptions::default()
            })
            .unwrap();
        assert_eq!(unmerged.len(), 2);
        assert!(unmerged.iter().all(|entry| entry.stage().unwrap() > 0));
    }

    #[test]
    fn expands_sparse_directories_unless_requested() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"value").unwrap();
        let nested = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"a".to_vec(), blob).unwrap(),
                    TreeEntry::new(EntryMode::BlobExecutable, b"b".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let sparse = IndexEntry::new(b"dir".to_vec(), 0o040_000, nested, StatData::default())
            .unwrap()
            .with_skip_worktree(true);
        repository
            .write_index(&crate::Index::new(crate::IndexVersion::V3, vec![sparse]).unwrap())
            .unwrap();

        let expanded = repository.ls_files(&LsFilesOptions::default()).unwrap();
        assert_eq!(
            expanded.iter().map(LsFilesEntry::path).collect::<Vec<_>>(),
            vec![b"dir/a".as_slice(), b"dir/b".as_slice()]
        );
        assert!(expanded.iter().all(LsFilesEntry::skip_worktree));
        let sparse = repository
            .ls_files(&LsFilesOptions {
                show_sparse_directories: true,
                ..LsFilesOptions::default()
            })
            .unwrap();
        assert_eq!(sparse[0].path(), b"dir");
        assert_eq!(sparse[0].mode(), Some(0o040_000));
    }
}
