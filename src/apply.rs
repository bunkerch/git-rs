//! Bounded, storage-agnostic unified patch application.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::status::worktree_path;
use crate::{Error, Index, IndexEntry, ObjectId, ObjectKind, Repository, Result, StatData};

/// Validation, destination, and resource policy for patch application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyOptions {
    /// Validate the complete patch without changing repository state.
    pub check: bool,
    /// Update the index as well as the worktree, requiring both preimages to match.
    pub index: bool,
    /// Apply additions as deletions and deletions as additions.
    pub reverse: bool,
    pub max_patch_bytes: usize,
    pub max_files: usize,
    pub max_hunks: usize,
    pub max_file_size: usize,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            check: false,
            index: false,
            reverse: false,
            max_patch_bytes: 64 * 1024 * 1024,
            max_files: 100_000,
            max_hunks: 1_000_000,
            max_file_size: 1024 * 1024 * 1024,
        }
    }
}

/// Summary returned after validation or application.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplyReport {
    pub files: usize,
    pub hunks: usize,
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
}

#[derive(Clone, Debug)]
struct FilePatch {
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    hunks: Vec<Hunk>,
}

#[derive(Clone, Debug)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<PatchLine>,
}

#[derive(Clone, Debug)]
struct PatchLine {
    kind: u8,
    data: Vec<u8>,
}

#[derive(Clone)]
struct PlannedFile {
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
    new_mode: Option<u32>,
    data: Vec<u8>,
}

impl Repository {
    /// Apply a Git-style unified patch to the abstract worktree and optionally index.
    ///
    /// The entire patch and all preimages are validated before the first worktree
    /// mutation. Hunks use exact line-number and content matching; callers that
    /// want fuzzy placement can inspect or transform patches before this API.
    ///
    /// # Errors
    /// Returns an error for malformed or oversized patches, unsafe paths, binary
    /// patches, preimage mismatches, index/worktree disagreement, or storage errors.
    pub fn apply_patch(&self, patch: &[u8], options: &ApplyOptions) -> Result<ApplyReport> {
        if patch.len() > options.max_patch_bytes {
            return Err(Error::InvalidRepository(format!(
                "patch exceeds {} bytes",
                options.max_patch_bytes
            )));
        }
        let worktree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("cannot apply a patch to a bare repository".into())
        })?;
        let mut patches = parse_patch(patch, options)?;
        if options.reverse {
            for patch in &mut patches {
                reverse_patch(patch);
            }
        }
        let index = options.index.then(|| self.read_index()).transpose()?;
        let index_by_path = index.as_ref().map(|index| {
            index
                .entries()
                .iter()
                .map(|entry| (entry.path().to_vec(), entry))
                .collect::<BTreeMap<_, _>>()
        });
        let mut seen = BTreeSet::new();
        let mut plans = Vec::with_capacity(patches.len());
        let mut report = ApplyReport {
            files: patches.len(),
            hunks: patches.iter().map(|patch| patch.hunks.len()).sum(),
            ..ApplyReport::default()
        };
        for patch in &patches {
            validate_file_patch(patch)?;
            let mut patch_paths = patch
                .old_path
                .iter()
                .chain(patch.new_path.iter())
                .cloned()
                .collect::<BTreeSet<_>>();
            if patch_paths.iter().any(|path| seen.contains(path)) {
                return invalid_patch("overlapping file changes are not supported");
            }
            seen.append(&mut patch_paths);
            let (worktree_data, worktree_mode) =
                self.read_patch_preimage(worktree, patch, options.max_file_size)?;
            if let Some(entries) = &index_by_path {
                validate_index_preimage(
                    self,
                    entries,
                    patch,
                    &worktree_data,
                    worktree_mode,
                    options,
                )?;
            }
            let output = apply_hunks(&worktree_data, &patch.hunks, options.max_file_size)?;
            match (&patch.old_path, &patch.new_path) {
                (None, Some(_)) => report.created += 1,
                (Some(_), None) => report.deleted += 1,
                (Some(_), Some(_)) => report.modified += 1,
                (None, None) => unreachable!(),
            }
            plans.push(PlannedFile {
                old_path: patch.old_path.clone(),
                new_path: patch.new_path.clone(),
                new_mode: patch.new_mode.or(worktree_mode).or(Some(0o100_644)),
                data: output,
            });
        }
        if options.check {
            return Ok(report);
        }
        let index_data = index
            .as_ref()
            .map(|index| self.prepare_patch_index(index, &plans))
            .transpose()?;
        let index_lock = self.git_path("index.lock");
        if let Some(data) = &index_data {
            self.filesystem().write_new(&index_lock, data)?;
        }
        for plan in &plans {
            if let Err(error) = self.publish_patch_file(worktree, plan) {
                if index_data.is_some() {
                    let _ = self.filesystem().remove_file(&index_lock);
                }
                return Err(error);
            }
        }
        if index_data.is_some()
            && let Err(error) = self
                .filesystem()
                .rename(&index_lock, &self.git_path("index"))
        {
            let _ = self.filesystem().remove_file(&index_lock);
            return Err(error);
        }
        Ok(report)
    }

    fn read_patch_preimage(
        &self,
        root: &Path,
        patch: &FilePatch,
        max_file_size: usize,
    ) -> Result<(Vec<u8>, Option<u32>)> {
        let Some(old_path) = &patch.old_path else {
            let target = root.join(worktree_path(patch.new_path.as_deref().unwrap())?);
            if self.filesystem().exists(&target)? {
                return Err(Error::AlreadyExists(worktree_path(
                    patch.new_path.as_deref().unwrap(),
                )?));
            }
            ensure_safe_parents(self, root, patch.new_path.as_deref().unwrap())?;
            return Ok((Vec::new(), None));
        };
        ensure_safe_parents(self, root, old_path)?;
        let full = root.join(worktree_path(old_path)?);
        let metadata = self.filesystem().metadata(&full)?;
        let declared = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if declared > max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: metadata.len(),
                limit: max_file_size,
            });
        }
        let (data, mode) = if metadata.is_symlink() {
            (self.filesystem().read_link(&full)?, 0o120_000)
        } else if metadata.is_file() {
            (
                self.filesystem().read(&full)?,
                if metadata.is_executable() {
                    0o100_755
                } else {
                    0o100_644
                },
            )
        } else {
            return Err(Error::InvalidRepository(format!(
                "patch target `{}` is not a file",
                String::from_utf8_lossy(old_path)
            )));
        };
        if let Some(expected) = patch.old_mode
            && expected != mode
        {
            return invalid_patch("preimage mode does not match worktree");
        }
        if let Some(new) = &patch.new_path
            && new != old_path
        {
            ensure_safe_parents(self, root, new)?;
            let destination = root.join(worktree_path(new)?);
            if self.filesystem().exists(&destination)? {
                return Err(Error::AlreadyExists(worktree_path(new)?));
            }
        }
        Ok((data, Some(mode)))
    }

    fn publish_patch_file(&self, root: &Path, plan: &PlannedFile) -> Result<()> {
        if let Some(old) = &plan.old_path
            && plan.new_path.as_ref() != Some(old)
        {
            self.filesystem()
                .remove_file(&root.join(worktree_path(old)?))?;
        }
        let Some(new) = &plan.new_path else {
            return Ok(());
        };
        ensure_safe_parents(self, root, new)?;
        let full = root.join(worktree_path(new)?);
        if let Some(parent) = full.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        if self.filesystem().exists(&full)? {
            self.filesystem().remove_file(&full)?;
        }
        match plan.new_mode.unwrap() {
            0o120_000 => self.filesystem().create_symlink(&full, &plan.data),
            0o100_644 | 0o100_755 => {
                self.filesystem().write(&full, &plan.data)?;
                self.filesystem()
                    .set_executable(&full, plan.new_mode == Some(0o100_755))
            }
            mode => invalid_patch(format!("unsupported patch mode {mode:o}")),
        }
    }

    fn prepare_patch_index(&self, index: &Index, plans: &[PlannedFile]) -> Result<Vec<u8>> {
        let affected = plans
            .iter()
            .flat_map(|plan| plan.old_path.iter().chain(plan.new_path.iter()))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut entries = index
            .entries()
            .iter()
            .filter(|entry| !affected.contains(entry.path()))
            .cloned()
            .collect::<Vec<_>>();
        for plan in plans {
            let Some(path) = &plan.new_path else { continue };
            let id = self.write_object(ObjectKind::Blob, &plan.data)?;
            entries.push(IndexEntry::new(
                path.clone(),
                plan.new_mode.unwrap(),
                id,
                StatData::default(),
            )?);
        }
        Index::new(index.version(), entries)?.encode()
    }
}

fn validate_index_preimage(
    repository: &Repository,
    entries: &BTreeMap<Vec<u8>, &IndexEntry>,
    patch: &FilePatch,
    worktree_data: &[u8],
    worktree_mode: Option<u32>,
    options: &ApplyOptions,
) -> Result<()> {
    let Some(old_path) = &patch.old_path else {
        if entries.contains_key(patch.new_path.as_deref().unwrap()) {
            return invalid_patch("new path already exists in index");
        }
        return Ok(());
    };
    let entry = entries
        .get(old_path)
        .ok_or_else(|| Error::InvalidRepository("patch preimage is absent from index".into()))?;
    if entry.stage() != 0 || Some(entry.mode()) != worktree_mode {
        return invalid_patch("index and worktree modes differ");
    }
    let object = repository.read_object(entry.id(), options.max_file_size)?;
    if object.kind() != ObjectKind::Blob || object.data() != worktree_data {
        return invalid_patch("index and worktree contents differ");
    }
    Ok(())
}

fn ensure_safe_parents(repository: &Repository, root: &Path, path: &[u8]) -> Result<()> {
    let relative = worktree_path(path)?;
    let mut parent = relative.parent();
    while let Some(value) = parent {
        if value.as_os_str().is_empty() {
            break;
        }
        let full = root.join(value);
        match repository.filesystem().metadata(&full) {
            Ok(metadata) if !metadata.is_dir() => {
                return invalid_patch("patch path traverses a non-directory");
            }
            Ok(_) | Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        parent = value.parent();
    }
    Ok(())
}

fn validate_file_patch(patch: &FilePatch) -> Result<()> {
    if patch.old_path.is_none() && patch.new_path.is_none() {
        return invalid_patch("file patch has no path");
    }
    for path in patch.old_path.iter().chain(patch.new_path.iter()) {
        IndexEntry::new(
            path.clone(),
            0o100_644,
            ObjectId::null(),
            StatData::default(),
        )?;
    }
    for mode in patch.old_mode.iter().chain(patch.new_mode.iter()) {
        if !matches!(*mode, 0o100_644 | 0o100_755 | 0o120_000) {
            return invalid_patch(format!("unsupported patch mode {mode:o}"));
        }
    }
    if patch.hunks.is_empty()
        && patch.old_path == patch.new_path
        && patch.old_mode == patch.new_mode
    {
        return invalid_patch("file patch contains no change");
    }
    Ok(())
}

fn apply_hunks(input: &[u8], hunks: &[Hunk], max_size: usize) -> Result<Vec<u8>> {
    let lines = split_lines(input);
    let mut output = Vec::new();
    let mut cursor = 0usize;
    for hunk in hunks {
        let start = if hunk.old_count == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        if start < cursor || start > lines.len() {
            return invalid_patch("hunk location is outside its preimage");
        }
        output.extend(
            lines[cursor..start]
                .iter()
                .flat_map(|line| line.iter())
                .copied(),
        );
        let mut position = start;
        for line in &hunk.lines {
            if line.kind != b'+' {
                if lines.get(position).copied() != Some(line.data.as_slice()) {
                    return invalid_patch("hunk preimage does not match");
                }
                position += 1;
            }
            if line.kind != b'-' {
                output.extend_from_slice(&line.data);
            }
            if output.len() > max_size {
                return Err(Error::ObjectTooLarge {
                    declared: output.len() as u64,
                    limit: max_size,
                });
            }
        }
        cursor = position;
    }
    output.extend(lines[cursor..].iter().flat_map(|line| line.iter()).copied());
    if output.len() > max_size {
        return Err(Error::ObjectTooLarge {
            declared: output.len() as u64,
            limit: max_size,
        });
    }
    Ok(output)
}

fn parse_patch(data: &[u8], options: &ApplyOptions) -> Result<Vec<FilePatch>> {
    let lines = split_lines(data);
    let mut patches = Vec::new();
    let mut cursor = 0;
    let mut total_hunks = 0usize;
    while cursor < lines.len() {
        if !lines[cursor].starts_with(b"diff --git ") {
            cursor += 1;
            continue;
        }
        let (header_old, header_new) = parse_git_header(trim_lf(lines[cursor]))?;
        cursor += 1;
        let mut patch = FilePatch {
            old_path: Some(header_old),
            new_path: Some(header_new),
            old_mode: None,
            new_mode: None,
            hunks: Vec::new(),
        };
        while cursor < lines.len() && !lines[cursor].starts_with(b"diff --git ") {
            let line = trim_lf(lines[cursor]);
            if let Some(value) = line.strip_prefix(b"new file mode ") {
                patch.new_mode = Some(parse_mode(value)?);
                patch.old_path = None;
            } else if let Some(value) = line.strip_prefix(b"deleted file mode ") {
                patch.old_mode = Some(parse_mode(value)?);
                patch.new_path = None;
            } else if let Some(value) = line.strip_prefix(b"old mode ") {
                patch.old_mode = Some(parse_mode(value)?);
            } else if let Some(value) = line.strip_prefix(b"new mode ") {
                patch.new_mode = Some(parse_mode(value)?);
            } else if let Some(value) = line.strip_prefix(b"rename from ") {
                patch.old_path = Some(parse_path(value, None)?);
            } else if let Some(value) = line.strip_prefix(b"rename to ") {
                patch.new_path = Some(parse_path(value, None)?);
            } else if let Some(value) = line.strip_prefix(b"--- ") {
                patch.old_path = parse_marker_path(value, b"a/")?;
            } else if let Some(value) = line.strip_prefix(b"+++ ") {
                patch.new_path = parse_marker_path(value, b"b/")?;
            } else if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
                return invalid_patch("binary patches are not supported");
            } else if let Some(header) = line.strip_prefix(b"@@ ") {
                let mut hunk = parse_hunk_header(header)?;
                cursor += 1;
                while cursor < lines.len() {
                    let current = lines[cursor];
                    if current.starts_with(b"diff --git ") || current.starts_with(b"@@ ") {
                        break;
                    }
                    if current.starts_with(b"\\ No newline at end of file") {
                        let previous = hunk.lines.last_mut().ok_or_else(|| {
                            Error::InvalidRepository("orphan no-newline marker".into())
                        })?;
                        if previous.data.last() == Some(&b'\n') {
                            previous.data.pop();
                        }
                    } else if matches!(current.first(), Some(b' ' | b'+' | b'-')) {
                        hunk.lines.push(PatchLine {
                            kind: current[0],
                            data: current[1..].to_vec(),
                        });
                    } else {
                        break;
                    }
                    cursor += 1;
                }
                validate_hunk_counts(&hunk)?;
                patch.hunks.push(hunk);
                total_hunks = total_hunks
                    .checked_add(1)
                    .ok_or_else(|| Error::InvalidRepository("hunk count overflow".into()))?;
                if total_hunks > options.max_hunks {
                    return invalid_patch("patch exceeds hunk limit");
                }
                continue;
            }
            cursor += 1;
        }
        if patch.old_path.is_none() && patch.new_path.is_none() {
            return invalid_patch("file patch lacks ---/+++ or rename headers");
        }
        patches.push(patch);
        if patches.len() > options.max_files {
            return invalid_patch("patch exceeds file limit");
        }
    }
    if patches.is_empty() {
        return invalid_patch("patch contains no file changes");
    }
    Ok(patches)
}

fn parse_git_header(line: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let value = line
        .strip_prefix(b"diff --git ")
        .ok_or_else(|| Error::InvalidRepository("invalid diff header".into()))?;
    let (old, consumed) = parse_header_token(value)?;
    let remaining = value
        .get(consumed..)
        .ok_or_else(|| Error::InvalidRepository("truncated diff header".into()))?;
    let remaining = remaining
        .strip_prefix(b" ")
        .ok_or_else(|| Error::InvalidRepository("diff header lacks two paths".into()))?;
    let (new, consumed) = parse_header_token(remaining)?;
    if consumed != remaining.len() {
        return invalid_patch("unexpected data after diff paths");
    }
    let old = old
        .strip_prefix(b"a/")
        .ok_or_else(|| Error::InvalidRepository("old diff path lacks a/ prefix".into()))?
        .to_vec();
    let new = new
        .strip_prefix(b"b/")
        .ok_or_else(|| Error::InvalidRepository("new diff path lacks b/ prefix".into()))?
        .to_vec();
    Ok((old, new))
}

fn parse_header_token(value: &[u8]) -> Result<(Vec<u8>, usize)> {
    if value.first() != Some(&b'"') {
        let end = value
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(value.len());
        if end == 0 {
            return invalid_patch("empty path in diff header");
        }
        return Ok((value[..end].to_vec(), end));
    }
    let mut cursor = 1;
    while cursor < value.len() {
        match value[cursor] {
            b'\\' => cursor = cursor.saturating_add(2),
            b'"' => return Ok((unquote_path(&value[..=cursor])?, cursor + 1)),
            _ => cursor += 1,
        }
    }
    invalid_patch("unterminated quoted path in diff header")
}

fn parse_hunk_header(header: &[u8]) -> Result<Hunk> {
    let end = header
        .windows(3)
        .position(|part| part == b" @@")
        .ok_or_else(|| Error::InvalidRepository("invalid hunk header".into()))?;
    let ranges = std::str::from_utf8(&header[..end])
        .map_err(|_| Error::InvalidRepository("non-ASCII hunk header".into()))?;
    let mut values = ranges.split_whitespace();
    let (old_start, old_count) = parse_range(
        values
            .next()
            .filter(|v| v.starts_with('-'))
            .ok_or_else(|| Error::InvalidRepository("missing old hunk range".into()))?,
    )?;
    let (new_start, new_count) = parse_range(
        values
            .next()
            .filter(|v| v.starts_with('+'))
            .ok_or_else(|| Error::InvalidRepository("missing new hunk range".into()))?,
    )?;
    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
    })
}

fn parse_range(value: &str) -> Result<(usize, usize)> {
    let value = &value[1..];
    let (start, count) = value.split_once(',').map_or((value, "1"), |pair| pair);
    Ok((
        start
            .parse()
            .map_err(|_| Error::InvalidRepository("invalid hunk start".into()))?,
        count
            .parse()
            .map_err(|_| Error::InvalidRepository("invalid hunk count".into()))?,
    ))
}

fn validate_hunk_counts(hunk: &Hunk) -> Result<()> {
    let old = hunk.lines.iter().filter(|line| line.kind != b'+').count();
    let new = hunk.lines.iter().filter(|line| line.kind != b'-').count();
    if old != hunk.old_count || new != hunk.new_count {
        return invalid_patch("hunk line counts do not match header");
    }
    let _ = hunk.new_start;
    Ok(())
}

fn reverse_patch(patch: &mut FilePatch) {
    std::mem::swap(&mut patch.old_path, &mut patch.new_path);
    std::mem::swap(&mut patch.old_mode, &mut patch.new_mode);
    for hunk in &mut patch.hunks {
        std::mem::swap(&mut hunk.old_start, &mut hunk.new_start);
        std::mem::swap(&mut hunk.old_count, &mut hunk.new_count);
        for line in &mut hunk.lines {
            line.kind = match line.kind {
                b'+' => b'-',
                b'-' => b'+',
                value => value,
            };
        }
    }
}

fn parse_marker_path(value: &[u8], prefix: &[u8]) -> Result<Option<Vec<u8>>> {
    if value == b"/dev/null" {
        Ok(None)
    } else {
        parse_path(value, Some(prefix)).map(Some)
    }
}

fn parse_path(value: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    let value = value.split(|byte| *byte == b'\t').next().unwrap_or(value);
    let mut path = if value.starts_with(b"\"") {
        unquote_path(value)?
    } else {
        value.to_vec()
    };
    if let Some(prefix) = prefix {
        path = path
            .strip_prefix(prefix)
            .ok_or_else(|| Error::InvalidRepository("patch path lacks expected prefix".into()))?
            .to_vec();
    }
    Ok(path)
}

fn unquote_path(value: &[u8]) -> Result<Vec<u8>> {
    if value.len() < 2 || value.last() != Some(&b'"') {
        return invalid_patch("unterminated quoted path");
    }
    let mut output = Vec::new();
    let mut cursor = 1;
    while cursor + 1 < value.len() {
        if value[cursor] != b'\\' {
            output.push(value[cursor]);
            cursor += 1;
            continue;
        }
        cursor += 1;
        let byte = *value
            .get(cursor)
            .ok_or_else(|| Error::InvalidRepository("truncated path escape".into()))?;
        match byte {
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'"' | b'\\' => output.push(byte),
            b'0'..=b'7' => {
                if cursor + 2 >= value.len() - 1 {
                    return invalid_patch("truncated octal path escape");
                }
                let digits = &value[cursor..cursor + 3];
                let text = std::str::from_utf8(digits)
                    .map_err(|_| Error::InvalidRepository("invalid octal path escape".into()))?;
                output.push(
                    u8::from_str_radix(text, 8).map_err(|_| {
                        Error::InvalidRepository("invalid octal path escape".into())
                    })?,
                );
                cursor += 2;
            }
            _ => return invalid_patch("invalid quoted path escape"),
        }
        cursor += 1;
    }
    Ok(output)
}

fn parse_mode(value: &[u8]) -> Result<u32> {
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("invalid patch mode".into()))?;
    u32::from_str_radix(value, 8).map_err(|_| Error::InvalidRepository("invalid patch mode".into()))
}

fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|byte| *byte == b'\n').collect()
}
fn trim_lf(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}
fn invalid_patch<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(format!(
        "invalid patch: {}",
        message.into()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem};

    fn repository() -> (Repository, MemoryFileSystem) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/tracked"), b"one\ntwo\n").unwrap();
        fs.write(Path::new("repo/remove"), b"old\n").unwrap();
        repository.add(".").unwrap();
        (repository, fs)
    }

    #[test]
    fn applies_multiple_files_and_updates_index() {
        let (repository, fs) = repository();
        let patch = b"diff --git a/tracked b/tracked\n--- a/tracked\n+++ b/tracked\n@@ -1,2 +1,2 @@\n one\n-two\n+changed\ndiff --git a/created b/created\nnew file mode 100755\n--- /dev/null\n+++ b/created\n@@ -0,0 +1 @@\n+new\ndiff --git a/remove b/remove\ndeleted file mode 100644\n--- a/remove\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n";
        let report = repository
            .apply_patch(
                patch,
                &ApplyOptions {
                    index: true,
                    ..ApplyOptions::default()
                },
            )
            .unwrap();
        assert_eq!(report.files, 3);
        assert_eq!(report.hunks, 3);
        assert_eq!((report.created, report.modified, report.deleted), (1, 1, 1));
        assert_eq!(
            fs.read(Path::new("repo/tracked")).unwrap(),
            b"one\nchanged\n"
        );
        assert_eq!(fs.read(Path::new("repo/created")).unwrap(), b"new\n");
        assert!(
            fs.metadata(Path::new("repo/created"))
                .unwrap()
                .is_executable()
        );
        assert!(matches!(
            fs.read(Path::new("repo/remove")),
            Err(Error::NotFound(_))
        ));
        let index = repository.read_index().unwrap();
        assert_eq!(index.entries().len(), 2);
        for entry in index.entries() {
            let object = repository.read_object(entry.id(), 1024).unwrap();
            assert_eq!(object.kind(), ObjectKind::Blob);
            assert_eq!(
                object.data(),
                fs.read(&Path::new("repo").join(worktree_path(entry.path()).unwrap()))
                    .unwrap()
            );
        }
    }

    #[test]
    fn check_and_failed_later_hunk_never_mutate() {
        let (repository, fs) = repository();
        let valid = b"diff --git a/tracked b/tracked\n--- a/tracked\n+++ b/tracked\n@@ -1,2 +1,2 @@\n one\n-two\n+changed\n";
        let report = repository
            .apply_patch(
                valid,
                &ApplyOptions {
                    check: true,
                    ..ApplyOptions::default()
                },
            )
            .unwrap();
        assert_eq!(report.modified, 1);
        assert_eq!(fs.read(Path::new("repo/tracked")).unwrap(), b"one\ntwo\n");

        let invalid = b"diff --git a/tracked b/tracked\n--- a/tracked\n+++ b/tracked\n@@ -1,2 +1,2 @@\n one\n-two\n+changed\ndiff --git a/remove b/remove\n--- a/remove\n+++ b/remove\n@@ -1 +1 @@\n-wrong\n+new\n";
        assert!(
            repository
                .apply_patch(invalid, &ApplyOptions::default())
                .is_err()
        );
        assert_eq!(fs.read(Path::new("repo/tracked")).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn reverse_and_no_newline_markers_round_trip() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/file"), b"old").unwrap();
        let patch = b"diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        repository
            .apply_patch(patch, &ApplyOptions::default())
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"new");
        repository
            .apply_patch(
                patch,
                &ApplyOptions {
                    reverse: true,
                    ..ApplyOptions::default()
                },
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"old");
    }

    #[test]
    fn rejects_unsafe_paths_and_index_disagreement() {
        let (repository, fs) = repository();
        let unsafe_patch = b"diff --git a/../escape b/../escape\n--- a/../escape\n+++ b/../escape\n@@ -0,0 +1 @@\n+x\n";
        assert!(
            repository
                .apply_patch(unsafe_patch, &ApplyOptions::default())
                .is_err()
        );

        fs.write(Path::new("repo/tracked"), b"local\n").unwrap();
        let patch = b"diff --git a/tracked b/tracked\n--- a/tracked\n+++ b/tracked\n@@ -1 +1 @@\n-local\n+new\n";
        assert!(
            repository
                .apply_patch(
                    patch,
                    &ApplyOptions {
                        index: true,
                        ..ApplyOptions::default()
                    }
                )
                .is_err()
        );
        assert_eq!(fs.read(Path::new("repo/tracked")).unwrap(), b"local\n");
    }

    #[test]
    fn mode_only_and_quoted_paths_are_supported() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/a b"), b"value\n").unwrap();
        let patch = b"diff --git \"a/a b\" \"b/a b\"\nold mode 100644\nnew mode 100755\n";
        repository
            .apply_patch(patch, &ApplyOptions::default())
            .unwrap();
        assert!(fs.metadata(Path::new("repo/a b")).unwrap().is_executable());
    }
}
