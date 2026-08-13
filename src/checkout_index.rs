//! Copy index entries through the abstract filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::fs::path::validate_path;
use crate::worktree::worktree_path;
use crate::{Error, FileStat, IndexEntry, ObjectKind, Repository, Result, StatData};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CheckoutIndexStage {
    #[default]
    Normal,
    Base,
    Ours,
    Theirs,
    AllConflicts,
}

impl CheckoutIndexStage {
    const fn number(self) -> Option<u8> {
        match self {
            Self::Normal => Some(0),
            Self::Base => Some(1),
            Self::Ours => Some(2),
            Self::Theirs => Some(3),
            Self::AllConflicts => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CheckoutIndexOptions {
    pub all: bool,
    pub force: bool,
    pub no_create: bool,
    pub update_stat: bool,
    pub ignore_skip_worktree: bool,
    pub temporary: bool,
    pub stage: CheckoutIndexStage,
    pub prefix: Vec<u8>,
    pub max_entries: usize,
    pub max_object_size: usize,
    pub max_total_bytes: usize,
    pub max_temp_attempts: usize,
}

impl Default for CheckoutIndexOptions {
    fn default() -> Self {
        Self {
            all: false,
            force: false,
            no_create: false,
            update_stat: false,
            ignore_skip_worktree: false,
            temporary: false,
            stage: CheckoutIndexStage::Normal,
            prefix: Vec::new(),
            max_entries: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024 * 1024,
            max_temp_attempts: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckoutIndexEntry {
    path: Vec<u8>,
    stage: u8,
    destination: PathBuf,
    temporary: bool,
}

impl CheckoutIndexEntry {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
    #[must_use]
    pub const fn stage(&self) -> u8 {
        self.stage
    }
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
    #[must_use]
    pub const fn is_temporary(&self) -> bool {
        self.temporary
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CheckoutIndexResult {
    entries: Vec<CheckoutIndexEntry>,
    output: Vec<u8>,
}

impl CheckoutIndexResult {
    #[must_use]
    pub fn entries(&self) -> &[CheckoutIndexEntry] {
        &self.entries
    }
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }
}

#[derive(Clone)]
struct Selected {
    entry: IndexEntry,
    destination: Option<PathBuf>,
    data: Vec<u8>,
}

impl Repository {
    /// Copy exact paths, or all applicable entries, from the index.
    ///
    /// Stage-all implies temporary regular files. Explicit paths and all
    /// selection are mutually exclusive.
    ///
    /// # Errors
    /// Returns an error for unsafe selection/destinations, collisions, missing
    /// stages, corrupt objects or index data, bare repositories, and limits.
    pub fn checkout_index(
        &self,
        paths: &[Vec<u8>],
        options: &CheckoutIndexOptions,
    ) -> Result<CheckoutIndexResult> {
        if options.all && !paths.is_empty() {
            return fail("all conflicts with explicit paths");
        }
        if options.stage == CheckoutIndexStage::AllConflicts && options.update_stat {
            return fail("stage-all cannot update index stat data");
        }
        validate_prefix(&options.prefix)?;
        let root = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("checkout-index requires a worktree".into()))?;
        let index = self.read_index()?;
        let requested = paths.iter().map(Vec::as_slice).collect::<BTreeSet<_>>();
        let mut seen = BTreeSet::new();
        let mut selected = Vec::new();
        let mut total = 0usize;
        for entry in index.entries() {
            if !options.all && !requested.contains(entry.path()) {
                continue;
            }
            seen.insert(entry.path());
            if entry.skip_worktree() && !options.ignore_skip_worktree {
                continue;
            }
            if !options
                .stage
                .number()
                .map_or(entry.stage() != 0, |stage| entry.stage() == stage)
            {
                continue;
            }
            if entry.mode() == 0o040_000 {
                if options.ignore_skip_worktree {
                    for leaf in self.flattened_tree(entry.id(), options.max_object_size)? {
                        let mut path = entry.path().to_vec();
                        path.push(b'/');
                        path.extend_from_slice(&leaf.path);
                        let expanded = IndexEntry::with_stage(
                            path,
                            leaf.raw_mode,
                            leaf.id,
                            StatData::default(),
                            entry.stage(),
                        )?;
                        self.prepare_copy(expanded, root, options, &mut selected, &mut total)?;
                    }
                }
            } else {
                self.prepare_copy(entry.clone(), root, options, &mut selected, &mut total)?;
            }
            if selected.len() > options.max_entries {
                return fail("entry count exceeds limit");
            }
        }
        if !options.all {
            for path in requested {
                if !seen.contains(path) {
                    return Err(Error::NotFound(worktree_path(path)?));
                }
                if options.stage != CheckoutIndexStage::AllConflicts
                    && !selected.iter().any(|item| item.entry.path() == path)
                {
                    return fail("path has no requested stage");
                }
            }
        }
        preflight(self, &selected, options)?;
        let result = self.materialize_copies(root, selected, options)?;
        if options.update_stat && !options.temporary {
            self.refresh_stats(index, &result.entries)?;
        }
        Ok(result)
    }

    fn prepare_copy(
        &self,
        entry: IndexEntry,
        root: &Path,
        options: &CheckoutIndexOptions,
        output: &mut Vec<Selected>,
        total: &mut usize,
    ) -> Result<()> {
        let object = self.read_object(entry.id(), options.max_object_size)?;
        let expected = if entry.mode() == 0o160_000 {
            ObjectKind::Commit
        } else {
            ObjectKind::Blob
        };
        if object.kind() != expected {
            return fail("index entry references wrong object kind");
        }
        *total = total
            .checked_add(object.data().len())
            .ok_or_else(|| Error::InvalidRepository("size overflow".into()))?;
        if *total > options.max_total_bytes {
            return fail("total bytes exceed limit");
        }
        let temporary = options.temporary || options.stage == CheckoutIndexStage::AllConflicts;
        let destination = if temporary {
            None
        } else {
            let mut value = options.prefix.clone();
            value.extend_from_slice(entry.path());
            validate_path(&value)?;
            Some(root.join(worktree_path(&value)?))
        };
        output.push(Selected {
            entry,
            destination,
            data: object.data().to_vec(),
        });
        Ok(())
    }

    fn materialize_copies(
        &self,
        root: &Path,
        selected: Vec<Selected>,
        options: &CheckoutIndexOptions,
    ) -> Result<CheckoutIndexResult> {
        let temporary = options.temporary || options.stage == CheckoutIndexStage::AllConflicts;
        let mut result = CheckoutIndexResult::default();
        let mut conflicts: BTreeMap<Vec<u8>, [Option<PathBuf>; 3]> = BTreeMap::new();
        for item in selected {
            let destination = if temporary {
                self.create_temp(root, &item.data, options, result.entries.len())?
            } else {
                let path = item.destination.expect("prepared destination");
                if options.no_create && !self.filesystem().exists(&path)? {
                    continue;
                }
                write_value(self, &path, &item.entry, &item.data, options.force)?;
                path
            };
            if temporary && item.entry.stage() != 0 {
                conflicts.entry(item.entry.path().to_vec()).or_default()
                    [usize::from(item.entry.stage() - 1)] = Some(destination.clone());
            }
            result.entries.push(CheckoutIndexEntry {
                path: item.entry.path().to_vec(),
                stage: item.entry.stage(),
                destination,
                temporary,
            });
        }
        if temporary {
            if options.stage == CheckoutIndexStage::AllConflicts {
                for (path, stages) in conflicts {
                    for (index, stage) in stages.iter().enumerate() {
                        if index != 0 {
                            result.output.push(b' ');
                        }
                        match stage {
                            Some(value) => append_temp_path(&mut result.output, root, value)?,
                            None => result.output.push(b'.'),
                        }
                    }
                    result.output.push(b'\t');
                    quote_path(&mut result.output, &path);
                    result.output.push(b'\n');
                }
            } else {
                for entry in &result.entries {
                    append_temp_path(&mut result.output, root, &entry.destination)?;
                    result.output.push(b'\t');
                    quote_path(&mut result.output, &entry.path);
                    result.output.push(b'\n');
                }
            }
        }
        Ok(result)
    }

    fn create_temp(
        &self,
        root: &Path,
        data: &[u8],
        options: &CheckoutIndexOptions,
        seed: usize,
    ) -> Result<PathBuf> {
        for attempt in 0..options.max_temp_attempts {
            let value = seed
                .checked_add(attempt)
                .ok_or_else(|| Error::InvalidRepository("temporary name overflow".into()))?;
            let path = root.join(format!(".git-rs-checkout-index-{value:016x}"));
            match self.filesystem().write_new(&path, data) {
                Ok(()) => return Ok(path),
                Err(Error::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        fail("temporary name attempts exhausted")
    }

    fn refresh_stats(&self, index: crate::Index, written: &[CheckoutIndexEntry]) -> Result<()> {
        let mut entries = Vec::with_capacity(index.entries().len());
        for entry in index.entries() {
            if let Some(copied) = written
                .iter()
                .find(|copy| copy.path == entry.path() && copy.stage == entry.stage())
            {
                let metadata = self.filesystem().metadata(copied.destination())?;
                entries.push(rebuild(entry, stat_data(metadata.stat(), metadata.len()))?);
            } else {
                entries.push(entry.clone());
            }
        }
        self.write_index(&index.with_entries(entries)?)
    }
}

fn preflight(
    repository: &Repository,
    selected: &[Selected],
    options: &CheckoutIndexOptions,
) -> Result<()> {
    if options.temporary || options.stage == CheckoutIndexStage::AllConflicts {
        return Ok(());
    }
    let mut paths = BTreeSet::new();
    for item in selected {
        let path = item.destination.as_ref().expect("prepared destination");
        if !paths.insert(path.clone()) {
            return fail("duplicate checkout destination");
        }
    }
    for path in &paths {
        if path
            .ancestors()
            .skip(1)
            .any(|parent| paths.contains(parent))
        {
            return fail("checkout-index destinations overlap as file and directory");
        }
        for parent in path.ancestors().skip(1) {
            match repository.filesystem().metadata(parent) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(Error::AlreadyExists(parent.to_path_buf()));
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        match repository.filesystem().metadata(path) {
            Ok(_) if !options.force => return Err(Error::AlreadyExists(path.clone())),
            Ok(metadata) if metadata.is_dir() => {
                if !repository.filesystem().read_dir(path)?.is_empty() {
                    return Err(Error::AlreadyExists(path.clone()));
                }
            }
            Ok(_) | Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_value(
    repository: &Repository,
    path: &Path,
    entry: &IndexEntry,
    data: &[u8],
    force: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        repository.filesystem().create_dir_all(parent)?;
    }
    if force && let Ok(metadata) = repository.filesystem().metadata(path) {
        if metadata.is_dir() {
            repository.filesystem().remove_dir(path)?;
        } else {
            repository.filesystem().remove_file(path)?;
        }
    }
    match entry.mode() {
        0o120_000 => repository.filesystem().create_symlink(path, data),
        0o160_000 => repository.filesystem().create_dir_all(path),
        0o100_644 | 0o100_755 => {
            repository.filesystem().write(path, data)?;
            repository
                .filesystem()
                .set_executable(path, entry.mode() == 0o100_755)
        }
        _ => fail("unsupported checkout-index mode"),
    }
}

fn validate_prefix(prefix: &[u8]) -> Result<()> {
    if prefix.starts_with(b"/")
        || prefix.contains(&0)
        || prefix
            .split(|byte| *byte == b'/')
            .any(|part| matches!(part, b"." | b".."))
    {
        return Err(Error::InvalidPath(PathBuf::from(
            String::from_utf8_lossy(prefix).into_owned(),
        )));
    }
    Ok(())
}

fn rebuild(entry: &IndexEntry, stat: StatData) -> Result<IndexEntry> {
    Ok(IndexEntry::with_stage(
        entry.path().to_vec(),
        entry.mode(),
        entry.id(),
        stat,
        entry.stage(),
    )?
    .with_assume_valid(entry.assume_valid())
    .with_intent_to_add(entry.intent_to_add())
    .with_skip_worktree(entry.skip_worktree()))
}

fn stat_data(stat: FileStat, len: u64) -> StatData {
    StatData {
        ctime_seconds: stat.ctime_seconds,
        ctime_nanoseconds: stat.ctime_nanoseconds,
        mtime_seconds: stat.mtime_seconds,
        mtime_nanoseconds: stat.mtime_nanoseconds,
        device: stat.device,
        inode: stat.inode,
        uid: stat.uid,
        gid: stat.gid,
        size: u32::try_from(len & u64::from(u32::MAX)).expect("value was masked to u32"),
    }
}

#[cfg_attr(unix, allow(clippy::unnecessary_wraps))]
fn append_os_path(output: &mut Vec<u8>, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        output.extend_from_slice(path.as_os_str().as_bytes());
        Ok(())
    }
    #[cfg(not(unix))]
    {
        output.extend_from_slice(
            path.to_str()
                .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))?
                .as_bytes(),
        );
        Ok(())
    }
}

fn append_temp_path(output: &mut Vec<u8>, root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| Error::InvalidPath(path.to_path_buf()))?;
    append_os_path(output, relative)
}

fn quote_path(output: &mut Vec<u8>, path: &[u8]) {
    let quote = path
        .iter()
        .any(|byte| !matches!(*byte, b'!'..=b'~') || matches!(*byte, b'"' | b'\\'));
    if !quote {
        output.extend_from_slice(path);
        return;
    }
    output.push(b'"');
    for byte in path {
        match *byte {
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'"' => output.extend_from_slice(b"\\\""),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x20..=0x7e => output.push(*byte),
            byte => output.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    output.push(b'"');
}

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{CheckoutIndexOptions, CheckoutIndexStage};
    use crate::{
        FileSystem, Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, ObjectKind,
        Repository, StatData,
    };
    use std::path::Path;

    #[test]
    fn copies_modes_prefixes_no_create_and_updates_stats() {
        let (repository, fs) = repository();
        let regular = repository
            .write_object(ObjectKind::Blob, b"regular")
            .unwrap();
        let executable = repository.write_object(ObjectKind::Blob, b"run").unwrap();
        let link = repository
            .write_object(ObjectKind::Blob, b"bin/run")
            .unwrap();
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::new(b"a".to_vec(), 0o100_644, regular, StatData::default())
                            .unwrap(),
                        IndexEntry::new(
                            b"bin/run".to_vec(),
                            0o100_755,
                            executable,
                            StatData::default(),
                        )
                        .unwrap(),
                        IndexEntry::new(b"link".to_vec(), 0o120_000, link, StatData::default())
                            .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        let result = repository
            .checkout_index(
                &[],
                &CheckoutIndexOptions {
                    all: true,
                    prefix: b"export/".to_vec(),
                    update_stat: true,
                    ..CheckoutIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.entries().len(), 3);
        assert_eq!(fs.read(Path::new("repo/export/a")).unwrap(), b"regular");
        assert_eq!(fs.read(Path::new("repo/export/bin/run")).unwrap(), b"run");
        assert!(
            fs.metadata(Path::new("repo/export/bin/run"))
                .unwrap()
                .is_executable()
        );
        assert_eq!(
            fs.read_link(Path::new("repo/export/link")).unwrap(),
            b"bin/run"
        );
        assert_eq!(repository.read_index().unwrap().entries()[0].stat().size, 7);

        fs.write(Path::new("repo/a"), b"old").unwrap();
        let copied = repository
            .checkout_index(
                &[],
                &CheckoutIndexOptions {
                    all: true,
                    force: true,
                    no_create: true,
                    ..CheckoutIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(copied.entries().len(), 1);
        assert_eq!(fs.read(Path::new("repo/a")).unwrap(), b"regular");
        assert!(!fs.exists(Path::new("repo/bin/run")).unwrap());
    }

    #[test]
    fn preflights_collisions_and_honors_skip_worktree() {
        let (repository, fs) = repository();
        let id = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V3,
                    vec![
                        IndexEntry::new(b"a".to_vec(), 0o100_644, id, StatData::default()).unwrap(),
                        IndexEntry::new(b"b".to_vec(), 0o100_644, id, StatData::default())
                            .unwrap()
                            .with_skip_worktree(true),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        fs.write(Path::new("repo/a"), b"local").unwrap();
        assert!(
            repository
                .checkout_index(
                    &[],
                    &CheckoutIndexOptions {
                        all: true,
                        ..CheckoutIndexOptions::default()
                    },
                )
                .is_err()
        );
        assert!(!fs.exists(Path::new("repo/b")).unwrap());
        fs.remove_file(Path::new("repo/a")).unwrap();
        let result = repository
            .checkout_index(
                &[],
                &CheckoutIndexOptions {
                    all: true,
                    ..CheckoutIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.entries().len(), 1);
        assert!(!fs.exists(Path::new("repo/b")).unwrap());
        repository
            .checkout_index(
                &[b"b".to_vec()],
                &CheckoutIndexOptions {
                    ignore_skip_worktree: true,
                    ..CheckoutIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/b")).unwrap(), b"x");
    }

    #[test]
    fn stage_all_writes_regular_temporary_files_and_records_missing_stage() {
        let (repository, fs) = repository();
        let base = repository.write_object(ObjectKind::Blob, b"base").unwrap();
        let theirs = repository
            .write_object(ObjectKind::Blob, b"target")
            .unwrap();
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::with_stage(
                            b"conflict".to_vec(),
                            0o100_644,
                            base,
                            StatData::default(),
                            1,
                        )
                        .unwrap(),
                        IndexEntry::with_stage(
                            b"conflict".to_vec(),
                            0o120_000,
                            theirs,
                            StatData::default(),
                            3,
                        )
                        .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        let result = repository
            .checkout_index(
                &[],
                &CheckoutIndexOptions {
                    all: true,
                    stage: CheckoutIndexStage::AllConflicts,
                    ..CheckoutIndexOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.entries().len(), 2);
        assert!(String::from_utf8_lossy(result.output()).contains(" . "));
        assert_eq!(fs.read(result.entries()[0].destination()).unwrap(), b"base");
        assert_eq!(
            fs.read(result.entries()[1].destination()).unwrap(),
            b"target"
        );
        assert!(
            fs.metadata(result.entries()[1].destination())
                .unwrap()
                .is_file()
        );
        assert!(!fs.exists(Path::new("repo/conflict")).unwrap());
    }

    #[test]
    fn rejects_prefix_that_aliases_git_at_checkout_destination() {
        let (repository, _fs) = repository();
        let id = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::new(
                            b"hooks/pre-commit".to_vec(),
                            0o100_644,
                            id,
                            StatData::default(),
                        )
                        .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        for prefix in [
            b"sub/.git./".as_slice(),
            b".git::$INDEX_ALLOCATION/".as_slice(),
        ] {
            assert!(
                repository
                    .checkout_index(
                        &[],
                        &CheckoutIndexOptions {
                            all: true,
                            prefix: prefix.to_vec(),
                            ..CheckoutIndexOptions::default()
                        },
                    )
                    .is_err(),
                "expected prefix {prefix:?} to be rejected at the destination"
            );
        }
    }

    fn repository() -> (Repository, MemoryFileSystem) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        (repository, fs)
    }
}
