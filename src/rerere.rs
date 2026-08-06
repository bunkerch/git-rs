//! Reuse recorded conflict resolutions through Git-compatible rr-cache data.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::object::sha1;
use crate::worktree::worktree_path;
use crate::{
    AttributeSource, AttributeValue, CheckAttributesOptions, Error, Index, IndexEntry,
    MergeFileOptions, ObjectId, ObjectKind, Repository, Result, StatData, merge_file,
};

const DEFAULT_MARKER_SIZE: usize = 7;

/// Mutation and resource policy for one rerere pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RerereOptions {
    /// Stage paths resolved from recorded postimages.
    pub autoupdate: bool,
    pub max_paths: usize,
    pub max_file_size: usize,
    pub max_variants: usize,
    pub max_conflict_depth: usize,
    pub max_merge_output_size: usize,
    pub max_merge_lines: usize,
    pub max_merge_trace_cells: usize,
    /// Bounds for `conflict-marker-size` attribute lookup. The source and
    /// selected attribute name are set by rerere.
    pub attributes: CheckAttributesOptions,
}

impl Default for RerereOptions {
    fn default() -> Self {
        Self {
            autoupdate: false,
            max_paths: 1_000_000,
            max_file_size: 1024 * 1024 * 1024,
            max_variants: 1_000_000,
            max_conflict_depth: 1024,
            max_merge_output_size: 1024 * 1024 * 1024,
            max_merge_lines: 10_000_000,
            max_merge_trace_cells: 100_000_000,
            attributes: CheckAttributesOptions::default(),
        }
    }
}

/// Paths changed or still tracked by one rerere pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RerereReport {
    recorded_preimages: Vec<Vec<u8>>,
    recorded_resolutions: Vec<Vec<u8>>,
    reused: Vec<Vec<u8>>,
    staged: Vec<Vec<u8>>,
    remaining: Vec<Vec<u8>>,
}

impl RerereReport {
    #[must_use]
    pub fn recorded_preimages(&self) -> &[Vec<u8>] {
        &self.recorded_preimages
    }

    #[must_use]
    pub fn recorded_resolutions(&self) -> &[Vec<u8>] {
        &self.recorded_resolutions
    }

    #[must_use]
    pub fn reused(&self) -> &[Vec<u8>] {
        &self.reused
    }

    #[must_use]
    pub fn staged(&self) -> &[Vec<u8>] {
        &self.staged
    }

    #[must_use]
    pub fn remaining(&self) -> &[Vec<u8>] {
        &self.remaining
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolutionId {
    hash: ObjectId,
    variant: usize,
}

#[derive(Clone, Debug)]
struct NormalizedConflict {
    data: Vec<u8>,
    hash: Option<ObjectId>,
}

impl Repository {
    /// Record new resolutions and reuse compatible recorded resolutions for
    /// paths in the current unmerged index.
    ///
    /// # Errors
    /// Returns an error for a bare repository, malformed index/cache state,
    /// invalid conflict markers, corrupt objects, or exceeded resource limits.
    pub fn rerere(&self, options: &RerereOptions) -> Result<RerereReport> {
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("rerere requires a worktree".into()))?;
        self.filesystem()
            .create_dir_all(&self.git_path("rr-cache"))?;
        let index = self.read_index()?;
        let conflicts = rerere_conflicts(&index, options.max_paths)?;
        let mut state = self.read_rerere_state(options.max_paths, options.max_variants)?;
        let mut paths = conflicts.iter().cloned().collect::<BTreeSet<_>>();
        paths.extend(state.keys().cloned());
        if paths.len() > options.max_paths {
            return rerere_error("rerere path count exceeds limit");
        }
        let marker_sizes = self.rerere_marker_sizes(&paths, options)?;
        let mut report = RerereReport::default();
        let mut paths_to_stage = BTreeSet::new();

        for path in paths {
            let full_path = work_tree.join(worktree_path(&path)?);
            let contents = match self.filesystem().read(&full_path) {
                Ok(contents) => contents,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if contents.len() > options.max_file_size {
                return rerere_error("rerere worktree file exceeds size limit");
            }
            let normalized =
                normalize_conflicts(&contents, marker_sizes[&path], options.max_conflict_depth)?;
            let Some(hash) = normalized.hash else {
                if let Some(id) = state.remove(&path) {
                    self.write_rerere_image(id, "postimage", &contents)?;
                    report.recorded_resolutions.push(path);
                }
                continue;
            };
            if !conflicts.contains(&path) {
                continue;
            }

            let variants = self.rerere_variants(hash, options.max_variants)?;
            let mut reused = None;
            for variant in &variants {
                if !variant.has_preimage || !variant.has_postimage {
                    continue;
                }
                let id = ResolutionId {
                    hash,
                    variant: variant.number,
                };
                if let Some(result) = self.try_rerere_resolution(id, &normalized.data, options)? {
                    self.filesystem().write(&full_path, &result)?;
                    reused = Some(id);
                    break;
                }
            }
            if reused.is_some() {
                state.remove(&path);
                if options.autoupdate {
                    paths_to_stage.insert(path.clone());
                    report.staged.push(path.clone());
                }
                report.reused.push(path);
                continue;
            }

            let id = if let Some(existing) = state.get(&path).copied().filter(|id| id.hash == hash)
            {
                existing
            } else if let Some(existing) = self.matching_unresolved_variant(
                hash,
                &normalized.data,
                &variants,
                options.max_file_size,
            )? {
                existing
            } else {
                ResolutionId {
                    hash,
                    variant: first_unused_variant(&variants, options.max_variants)?,
                }
            };
            self.write_rerere_image(id, "preimage", &normalized.data)?;
            self.remove_rerere_image(id, "postimage")?;
            state.insert(path.clone(), id);
            report.recorded_preimages.push(path);
        }

        if !paths_to_stage.is_empty() {
            self.stage_rerere_paths(index, &paths_to_stage, options.max_file_size)?;
        }
        self.write_rerere_state(&state)?;
        report.remaining = self.rerere_remaining(options.max_paths, options.max_variants)?;
        Ok(report)
    }

    /// Return paths currently recorded in `MERGE_RR`.
    ///
    /// # Errors
    /// Returns an error for malformed cache state or exceeded limits.
    pub fn rerere_status(&self, max_paths: usize, max_variants: usize) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .read_rerere_state(max_paths, max_variants)?
            .into_keys()
            .collect())
    }

    /// Return unresolved and unsupported paths remaining in the index.
    ///
    /// # Errors
    /// Returns an error for malformed index/cache state or exceeded limits.
    pub fn rerere_remaining(&self, max_paths: usize, max_variants: usize) -> Result<Vec<Vec<u8>>> {
        let index = self.read_index()?;
        let handled = rerere_conflicts(&index, max_paths)?;
        let resolved = index
            .entries()
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| entry.path().to_vec())
            .collect::<BTreeSet<_>>();
        let mut remaining = self
            .read_rerere_state(max_paths, max_variants)?
            .into_keys()
            .filter(|path| !resolved.contains(path))
            .collect::<BTreeSet<_>>();
        for group in index
            .entries()
            .chunk_by(|left, right| left.path() == right.path())
        {
            if group.iter().any(|entry| entry.stage() != 0) && !handled.contains(group[0].path()) {
                remaining.insert(group[0].path().to_vec());
                if remaining.len() > max_paths {
                    return rerere_error("rerere path count exceeds limit");
                }
            }
        }
        Ok(remaining.into_iter().collect())
    }

    /// Remove current-session state and discard preimages which have no saved
    /// resolution. Completed resolution pairs remain reusable.
    ///
    /// # Errors
    /// Returns an error for malformed cache state, exceeded limits, or storage
    /// failures.
    pub fn rerere_clear(&self, max_paths: usize, max_variants: usize) -> Result<()> {
        let state = self.read_rerere_state(max_paths, max_variants)?;
        for id in state.values() {
            if !self.rerere_image_exists(*id, "postimage")? {
                self.remove_rerere_image(*id, "preimage")?;
                self.remove_rerere_directory_if_empty(id.hash)?;
            }
        }
        match self.filesystem().remove_file(&self.git_path("MERGE_RR")) {
            Ok(()) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Forget the recorded resolution which applies to each selected current
    /// conflict and track its normalized preimage again.
    ///
    /// # Errors
    /// Returns an error for a missing path/resolution, malformed conflict data,
    /// or exceeded resource/storage limits.
    pub fn rerere_forget(&self, paths: &[Vec<u8>], options: &RerereOptions) -> Result<()> {
        if paths.is_empty() || paths.len() > options.max_paths {
            return rerere_error("rerere forget requires bounded nonempty paths");
        }
        let work_tree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("rerere requires a worktree".into()))?;
        let selected = paths.iter().cloned().collect::<BTreeSet<_>>();
        let marker_sizes = self.rerere_marker_sizes(&selected, options)?;
        let mut state = self.read_rerere_state(options.max_paths, options.max_variants)?;
        for path in paths {
            let contents = self
                .filesystem()
                .read(&work_tree.join(worktree_path(path)?))?;
            if contents.len() > options.max_file_size {
                return rerere_error("rerere worktree file exceeds size limit");
            }
            let normalized =
                normalize_conflicts(&contents, marker_sizes[path], options.max_conflict_depth)?;
            let hash = normalized
                .hash
                .ok_or_else(|| Error::InvalidRepository("path has no conflict markers".into()))?;
            let variants = self.rerere_variants(hash, options.max_variants)?;
            let mut forgotten = None;
            for variant in variants {
                if !variant.has_preimage || !variant.has_postimage {
                    continue;
                }
                let id = ResolutionId {
                    hash,
                    variant: variant.number,
                };
                if self
                    .try_rerere_resolution(id, &normalized.data, options)?
                    .is_some()
                {
                    self.remove_rerere_image(id, "postimage")?;
                    self.write_rerere_image(id, "preimage", &normalized.data)?;
                    forgotten = Some(id);
                    break;
                }
            }
            let id = forgotten.ok_or_else(|| {
                Error::InvalidRepository("no recorded resolution applies to path".into())
            })?;
            state.insert(path.clone(), id);
        }
        self.write_rerere_state(&state)
    }

    fn rerere_marker_sizes(
        &self,
        paths: &BTreeSet<Vec<u8>>,
        options: &RerereOptions,
    ) -> Result<BTreeMap<Vec<u8>, usize>> {
        let paths = paths.iter().cloned().collect::<Vec<_>>();
        let mut attribute_options = options.attributes.clone();
        attribute_options.source = AttributeSource::WorktreeThenIndex;
        attribute_options.attributes = vec!["conflict-marker-size".into()];
        attribute_options.max_paths = attribute_options.max_paths.min(options.max_paths);
        let results = self.check_attributes(&paths, &attribute_options)?;
        let mut output = paths
            .into_iter()
            .map(|path| (path, DEFAULT_MARKER_SIZE))
            .collect::<BTreeMap<_, _>>();
        for result in results {
            let size = match result.value() {
                AttributeValue::Value(value) => value
                    .parse::<usize>()
                    .ok()
                    .filter(|size| *size > 0)
                    .unwrap_or(DEFAULT_MARKER_SIZE),
                AttributeValue::Set | AttributeValue::Unset | AttributeValue::Unspecified => {
                    DEFAULT_MARKER_SIZE
                }
            };
            output.insert(result.path().to_vec(), size);
        }
        Ok(output)
    }

    fn try_rerere_resolution(
        &self,
        id: ResolutionId,
        current: &[u8],
        options: &RerereOptions,
    ) -> Result<Option<Vec<u8>>> {
        let preimage = self.read_rerere_image(id, "preimage", options.max_file_size)?;
        let postimage = self.read_rerere_image(id, "postimage", options.max_file_size)?;
        let result = merge_file(
            current,
            &preimage,
            &postimage,
            &MergeFileOptions {
                max_input_size: options.max_file_size,
                max_output_size: options.max_merge_output_size,
                max_lines: options.max_merge_lines,
                max_trace_cells: options.max_merge_trace_cells,
                ..MergeFileOptions::default()
            },
        )?;
        Ok((result.conflicts() == 0).then(|| result.data().to_vec()))
    }

    fn stage_rerere_paths(
        &self,
        index: Index,
        paths: &BTreeSet<Vec<u8>>,
        max_file_size: usize,
    ) -> Result<()> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("worktree required for rerere".into())
        })?;
        let mut entries = index.entries().to_vec();
        entries.retain(|entry| !paths.contains(entry.path()));
        for path in paths {
            let contents = self
                .filesystem()
                .read(&work_tree.join(worktree_path(path)?))?;
            if contents.len() > max_file_size {
                return rerere_error("rerere resolved file exceeds size limit");
            }
            let metadata = self
                .filesystem()
                .metadata(&work_tree.join(worktree_path(path)?))?;
            let mode = if metadata.is_executable() {
                0o100_755
            } else {
                0o100_644
            };
            let id = self.write_object(ObjectKind::Blob, &contents)?;
            entries.push(IndexEntry::new(
                path.clone(),
                mode,
                id,
                StatData::default(),
            )?);
        }
        self.write_index(&index.with_entries(entries)?)
    }

    fn read_rerere_state(
        &self,
        max_paths: usize,
        max_variants: usize,
    ) -> Result<BTreeMap<Vec<u8>, ResolutionId>> {
        let contents = match self.read_git_file("MERGE_RR") {
            Ok(contents) => contents,
            Err(Error::NotFound(_)) => return Ok(BTreeMap::new()),
            Err(error) => return Err(error),
        };
        let mut output = BTreeMap::new();
        for record in contents.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| Error::InvalidRepository("corrupt MERGE_RR record".into()))?;
            let id = parse_resolution_id(&record[..tab], max_variants)?;
            let path = record[tab + 1..].to_vec();
            validate_rerere_path(&path)?;
            if output.insert(path, id).is_some() || output.len() > max_paths {
                return rerere_error("duplicate or excessive MERGE_RR records");
            }
        }
        Ok(output)
    }

    fn write_rerere_state(&self, state: &BTreeMap<Vec<u8>, ResolutionId>) -> Result<()> {
        let mut contents = Vec::new();
        for (path, id) in state {
            contents.extend_from_slice(id.hash.to_string().as_bytes());
            if id.variant > 0 {
                contents.push(b'.');
                contents.extend_from_slice(id.variant.to_string().as_bytes());
            }
            contents.push(b'\t');
            contents.extend_from_slice(path);
            contents.push(0);
        }
        self.write_atomic(Path::new("MERGE_RR"), &contents)
    }

    fn rerere_variants(&self, hash: ObjectId, limit: usize) -> Result<Vec<VariantState>> {
        let directory = self.rerere_directory(hash);
        let names = match self.filesystem().read_dir(&directory) {
            Ok(names) => names,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut variants = BTreeMap::<usize, VariantState>::new();
        for name in names {
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some((kind, number)) = parse_image_name(name) else {
                continue;
            };
            if number >= limit {
                return rerere_error("rerere variant exceeds limit");
            }
            let state = variants.entry(number).or_insert(VariantState {
                number,
                has_preimage: false,
                has_postimage: false,
            });
            if kind == "preimage" {
                state.has_preimage = true;
            } else {
                state.has_postimage = true;
            }
            if variants.len() > limit {
                return rerere_error("rerere variant count exceeds limit");
            }
        }
        Ok(variants.into_values().collect())
    }

    fn matching_unresolved_variant(
        &self,
        hash: ObjectId,
        current: &[u8],
        variants: &[VariantState],
        max_size: usize,
    ) -> Result<Option<ResolutionId>> {
        for variant in variants {
            if variant.has_preimage && !variant.has_postimage {
                let id = ResolutionId {
                    hash,
                    variant: variant.number,
                };
                if self.read_rerere_image(id, "preimage", max_size)? == current {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    fn rerere_directory(&self, hash: ObjectId) -> PathBuf {
        self.git_path(Path::new("rr-cache").join(hash.to_string()))
    }

    fn rerere_image_path(&self, id: ResolutionId, kind: &str) -> PathBuf {
        let name = if id.variant == 0 {
            kind.to_owned()
        } else {
            format!("{kind}.{}", id.variant)
        };
        self.rerere_directory(id.hash).join(name)
    }

    fn read_rerere_image(&self, id: ResolutionId, kind: &str, max_size: usize) -> Result<Vec<u8>> {
        let contents = self.filesystem().read(&self.rerere_image_path(id, kind))?;
        if contents.len() > max_size {
            return rerere_error("rerere cache image exceeds size limit");
        }
        Ok(contents)
    }

    fn write_rerere_image(&self, id: ResolutionId, kind: &str, data: &[u8]) -> Result<()> {
        let directory = self.rerere_directory(id.hash);
        self.filesystem().create_dir_all(&directory)?;
        self.filesystem()
            .write(&self.rerere_image_path(id, kind), data)
    }

    fn remove_rerere_image(&self, id: ResolutionId, kind: &str) -> Result<()> {
        match self
            .filesystem()
            .remove_file(&self.rerere_image_path(id, kind))
        {
            Ok(()) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn rerere_image_exists(&self, id: ResolutionId, kind: &str) -> Result<bool> {
        self.filesystem().exists(&self.rerere_image_path(id, kind))
    }

    fn remove_rerere_directory_if_empty(&self, hash: ObjectId) -> Result<()> {
        match self.filesystem().remove_dir(&self.rerere_directory(hash)) {
            Ok(()) | Err(Error::NotFound(_) | Error::DirectoryNotEmpty(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct VariantState {
    number: usize,
    has_preimage: bool,
    has_postimage: bool,
}

struct ParsedConflict {
    normalized: Vec<u8>,
    end: usize,
    one: Vec<u8>,
    two: Vec<u8>,
}

fn rerere_conflicts(index: &Index, max_paths: usize) -> Result<BTreeSet<Vec<u8>>> {
    let mut output = BTreeSet::new();
    for group in index
        .entries()
        .chunk_by(|left, right| left.path() == right.path())
    {
        let ours = group.iter().find(|entry| entry.stage() == 2);
        let theirs = group.iter().find(|entry| entry.stage() == 3);
        if let (Some(ours), Some(theirs)) = (ours, theirs)
            && matches!(ours.mode(), 0o100_644 | 0o100_755)
            && matches!(theirs.mode(), 0o100_644 | 0o100_755)
        {
            output.insert(group[0].path().to_vec());
            if output.len() > max_paths {
                return rerere_error("rerere conflict path count exceeds limit");
            }
        }
    }
    Ok(output)
}

fn normalize_conflicts(
    data: &[u8],
    marker_size: usize,
    max_depth: usize,
) -> Result<NormalizedConflict> {
    if marker_size == 0 || max_depth == 0 {
        return rerere_error("conflict marker size and nesting limit must be positive");
    }
    let mut cursor = 0usize;
    let mut output = Vec::with_capacity(data.len());
    let mut hash_input = Vec::new();
    let mut found = false;
    while cursor < data.len() {
        let (line, next) = next_line(data, cursor);
        if is_marker(line, b'<', marker_size) {
            let conflict = parse_conflict(data, next, marker_size, 1, max_depth)?;
            output.extend_from_slice(&conflict.normalized);
            hash_input.extend_from_slice(&conflict.one);
            hash_input.push(0);
            hash_input.extend_from_slice(&conflict.two);
            hash_input.push(0);
            cursor = conflict.end;
            found = true;
        } else {
            output.extend_from_slice(line);
            cursor = next;
        }
    }
    Ok(NormalizedConflict {
        data: output,
        hash: found.then(|| ObjectId::from_bytes(sha1::digest(&hash_input))),
    })
}

fn parse_conflict(
    data: &[u8],
    mut cursor: usize,
    marker_size: usize,
    depth: usize,
    max_depth: usize,
) -> Result<ParsedConflict> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Side {
        One,
        Original,
        Two,
    }
    let mut side = Side::One;
    let mut one = Vec::new();
    let mut two = Vec::new();
    while cursor < data.len() {
        let (line, next) = next_line(data, cursor);
        if is_marker(line, b'<', marker_size) {
            if depth == max_depth {
                return rerere_error("nested conflict marker depth exceeds limit");
            }
            let nested = parse_conflict(data, next, marker_size, depth + 1, max_depth)?;
            if side == Side::One {
                one.extend_from_slice(&nested.normalized);
            } else {
                two.extend_from_slice(&nested.normalized);
            }
            cursor = nested.end;
            continue;
        }
        if is_marker(line, b'|', marker_size) {
            if side != Side::One {
                return rerere_error("malformed diff3 conflict markers");
            }
            side = Side::Original;
        } else if is_marker(line, b'=', marker_size) {
            if side == Side::Two {
                return rerere_error("duplicate conflict separator");
            }
            side = Side::Two;
        } else if is_marker(line, b'>', marker_size) {
            if side != Side::Two {
                return rerere_error("conflict end appears before separator");
            }
            if one > two {
                std::mem::swap(&mut one, &mut two);
            }
            let mut normalized = Vec::new();
            normalized.extend(std::iter::repeat_n(b'<', marker_size));
            normalized.push(b'\n');
            normalized.extend_from_slice(&one);
            normalized.extend(std::iter::repeat_n(b'=', marker_size));
            normalized.push(b'\n');
            normalized.extend_from_slice(&two);
            normalized.extend(std::iter::repeat_n(b'>', marker_size));
            normalized.push(b'\n');
            return Ok(ParsedConflict {
                normalized,
                end: next,
                one,
                two,
            });
        } else {
            match side {
                Side::One => one.extend_from_slice(line),
                Side::Original => {}
                Side::Two => two.extend_from_slice(line),
            }
        }
        cursor = next;
    }
    rerere_error("unterminated conflict markers")
}

fn next_line(data: &[u8], start: usize) -> (&[u8], usize) {
    let end = data[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(data.len(), |offset| start + offset + 1);
    (&data[start..end], end)
}

fn is_marker(line: &[u8], marker: u8, size: usize) -> bool {
    if line.len() <= size || !line[..size].iter().all(|byte| *byte == marker) {
        return false;
    }
    if matches!(marker, b'<' | b'>') && line[size] != b' ' {
        return false;
    }
    line[size].is_ascii_whitespace()
}

fn parse_resolution_id(value: &[u8], max_variants: usize) -> Result<ResolutionId> {
    let text = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("non-UTF-8 MERGE_RR ID".into()))?;
    let (hash, variant) = text.rsplit_once('.').map_or((text, 0), |(hash, variant)| {
        (hash, variant.parse::<usize>().unwrap_or(usize::MAX))
    });
    if variant >= max_variants {
        return rerere_error("MERGE_RR variant exceeds limit");
    }
    Ok(ResolutionId {
        hash: ObjectId::from_str(hash)?,
        variant,
    })
}

fn parse_image_name(name: &str) -> Option<(&str, usize)> {
    for kind in ["preimage", "postimage"] {
        if name == kind {
            return Some((kind, 0));
        }
        if let Some(suffix) = name
            .strip_prefix(kind)
            .and_then(|value| value.strip_prefix('.'))
            && let Ok(number) = suffix.parse()
        {
            return Some((kind, number));
        }
    }
    None
}

fn first_unused_variant(variants: &[VariantState], limit: usize) -> Result<usize> {
    let used = variants
        .iter()
        .filter(|variant| variant.has_preimage || variant.has_postimage)
        .map(|variant| variant.number)
        .collect::<BTreeSet<_>>();
    (0..limit)
        .find(|variant| !used.contains(variant))
        .ok_or_else(|| Error::InvalidRepository("rerere variant limit exhausted".into()))
}

fn validate_rerere_path(path: &[u8]) -> Result<()> {
    if path.is_empty()
        || path.starts_with(b"/")
        || path.ends_with(b"/")
        || path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|component| matches!(component, b"" | b"." | b".."))
    {
        return Err(Error::InvalidPath(
            String::from_utf8_lossy(path).into_owned().into(),
        ));
    }
    Ok(())
}

fn rerere_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidRepository(message.into()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{RerereOptions, normalize_conflicts};
    use crate::{
        FileSystem, Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, ObjectKind,
        Repository, StatData,
    };

    const CONFLICT: &[u8] = b"before\n<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\nafter\n";

    #[test]
    fn normalization_discards_base_orders_sides_and_hashes_only_hunks() {
        let first = normalize_conflicts(
            b"context\n<<<<<<< x\nz\n||||||| base\nold\n=======\na\n>>>>>>> y\n",
            7,
            64,
        )
        .unwrap();
        let second =
            normalize_conflicts(b"other\n<<<<<<< x\na\n=======\nz\n>>>>>>> y\n", 7, 64).unwrap();
        assert_eq!(first.hash, second.hash);
        assert_eq!(first.data, b"context\n<<<<<<<\na\n=======\nz\n>>>>>>>\n");
        assert!(normalize_conflicts(b"<<<<<<< x\na\n", 7, 64).is_err());
        assert!(
            normalize_conflicts(
                b"<<<<<<< x\n<<<<<<< n\na\n=======\nb\n>>>>>>> n\n=======\nc\n>>>>>>> x\n",
                7,
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn records_reuses_autostages_forgets_and_clears_resolutions() {
        let (repository, filesystem) = conflicted_repository(CONFLICT);
        let first = repository.rerere(&RerereOptions::default()).unwrap();
        assert_eq!(first.recorded_preimages(), &[b"file".to_vec()]);
        assert_eq!(repository.rerere_status(10, 10).unwrap(), [b"file"]);

        filesystem
            .write(Path::new("repo/file"), b"before\nresolved\nafter\n")
            .unwrap();
        let recorded = repository.rerere(&RerereOptions::default()).unwrap();
        assert_eq!(recorded.recorded_resolutions(), &[b"file".to_vec()]);
        assert!(repository.rerere_status(10, 10).unwrap().is_empty());

        filesystem
            .write(
                Path::new("repo/file"),
                b"new-before\n<<<<<<< current\nours\n=======\ntheirs\n>>>>>>> other\nnew-after\n",
            )
            .unwrap();
        let reused = repository
            .rerere(&RerereOptions {
                autoupdate: true,
                ..RerereOptions::default()
            })
            .unwrap();
        assert_eq!(reused.reused(), &[b"file".to_vec()]);
        assert_eq!(reused.staged(), &[b"file".to_vec()]);
        assert_eq!(
            filesystem.read(Path::new("repo/file")).unwrap(),
            b"new-before\nresolved\nnew-after\n"
        );
        let index = repository.read_index().unwrap();
        assert_eq!(index.entries().len(), 1);
        assert_eq!(index.entries()[0].stage(), 0);

        repository
            .write_index(&unmerged_index(&repository))
            .unwrap();
        filesystem.write(Path::new("repo/file"), CONFLICT).unwrap();
        repository
            .rerere_forget(&[b"file".to_vec()], &RerereOptions::default())
            .unwrap();
        assert_eq!(repository.rerere_status(10, 10).unwrap(), [b"file"]);
        repository.rerere_clear(10, 10).unwrap();
        assert!(repository.rerere_status(10, 10).unwrap().is_empty());
        assert!(
            filesystem
                .read_dir(Path::new("repo/.git/rr-cache"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn honors_conflict_marker_size_attribute_and_limits() {
        let conflict = b"<<<< ours\none\n====\ntwo\n>>>> theirs\n";
        let (repository, filesystem) = conflicted_repository(conflict);
        filesystem
            .write(
                Path::new("repo/.gitattributes"),
                b"file conflict-marker-size=4\n",
            )
            .unwrap();
        assert_eq!(
            repository
                .rerere(&RerereOptions::default())
                .unwrap()
                .recorded_preimages(),
            &[b"file".to_vec()]
        );
        assert!(
            repository
                .rerere(&RerereOptions {
                    max_file_size: 1,
                    ..RerereOptions::default()
                })
                .is_err()
        );
    }

    fn conflicted_repository(contents: &[u8]) -> (Repository, MemoryFileSystem) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        repository
            .write_index(&unmerged_index(&repository))
            .unwrap();
        filesystem.write(Path::new("repo/file"), contents).unwrap();
        (repository, filesystem)
    }

    fn unmerged_index(repository: &Repository) -> Index {
        let base = repository
            .write_object(ObjectKind::Blob, b"base\n")
            .unwrap();
        let ours = repository
            .write_object(ObjectKind::Blob, b"ours\n")
            .unwrap();
        let theirs = repository
            .write_object(ObjectKind::Blob, b"theirs\n")
            .unwrap();
        Index::new(
            IndexVersion::V2,
            vec![
                IndexEntry::with_stage(b"file".to_vec(), 0o100_644, base, StatData::default(), 1)
                    .unwrap(),
                IndexEntry::with_stage(b"file".to_vec(), 0o100_644, ours, StatData::default(), 2)
                    .unwrap(),
                IndexEntry::with_stage(b"file".to_vec(), 0o100_644, theirs, StatData::default(), 3)
                    .unwrap(),
            ],
        )
        .unwrap()
    }
}
