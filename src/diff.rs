//! Typed tree/index differences and bounded unified patch rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::{Error, Index, ObjectId, ObjectKind, Repository, Result};

/// Classification of one path-level difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffKind {
    Added,
    Deleted,
    Modified,
    TypeChanged,
    Renamed,
}

/// One old/new file pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffEntry {
    kind: DiffKind,
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    old_id: Option<ObjectId>,
    new_id: Option<ObjectId>,
}

impl DiffEntry {
    #[must_use]
    pub const fn kind(&self) -> DiffKind {
        self.kind
    }

    #[must_use]
    pub fn old_path(&self) -> Option<&[u8]> {
        self.old_path.as_deref()
    }

    #[must_use]
    pub fn new_path(&self) -> Option<&[u8]> {
        self.new_path.as_deref()
    }

    #[must_use]
    pub const fn old_mode(&self) -> Option<u32> {
        self.old_mode
    }

    #[must_use]
    pub const fn new_mode(&self) -> Option<u32> {
        self.new_mode
    }

    #[must_use]
    pub const fn old_id(&self) -> Option<ObjectId> {
        self.old_id
    }

    #[must_use]
    pub const fn new_id(&self) -> Option<ObjectId> {
        self.new_id
    }
}

/// Rename detection, object, and patch-algorithm limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffOptions {
    pub detect_exact_renames: bool,
    pub context_lines: usize,
    pub max_object_size: usize,
    pub max_lines: usize,
    pub max_trace_cells: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            detect_exact_renames: true,
            context_lines: 3,
            max_object_size: 1024 * 1024 * 1024,
            max_lines: 1_000_000,
            max_trace_cells: 10_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Value {
    mode: u32,
    id: ObjectId,
}

impl Repository {
    /// Compare two trees. `None` represents the empty tree.
    ///
    /// # Errors
    /// Returns an error for missing/corrupt trees, oversized objects, or unsafe
    /// tree paths.
    pub fn diff_trees(
        &self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        options: &DiffOptions,
    ) -> Result<Vec<DiffEntry>> {
        let old = self.optional_tree_map(old, options.max_object_size)?;
        let new = self.optional_tree_map(new, options.max_object_size)?;
        Ok(diff_maps(&old, &new, options.detect_exact_renames))
    }

    /// Compare a tree (or empty tree) to the stage-zero index.
    ///
    /// # Errors
    /// Returns an error for unmerged index entries or corrupt/oversized trees.
    pub fn diff_tree_to_index(
        &self,
        tree: Option<ObjectId>,
        index: &Index,
        options: &DiffOptions,
    ) -> Result<Vec<DiffEntry>> {
        if index.entries().iter().any(|entry| entry.stage() != 0) {
            return Err(Error::InvalidRepository(
                "cannot diff an index with unresolved stages".into(),
            ));
        }
        let old = self.optional_tree_map(tree, options.max_object_size)?;
        let new = index
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.path().to_vec(),
                    Value {
                        mode: entry.mode(),
                        id: entry.id(),
                    },
                )
            })
            .collect();
        Ok(diff_maps(&old, &new, options.detect_exact_renames))
    }

    /// Render one typed entry as a Git-style unified patch.
    ///
    /// Exact renames have headers but no content hunk. Gitlinks and blobs with
    /// NUL bytes receive a binary-difference line.
    ///
    /// # Errors
    /// Returns an error for corrupt/wrong-kind objects or line/trace limits.
    pub fn render_patch(&self, entry: &DiffEntry, options: &DiffOptions) -> Result<Vec<u8>> {
        let old_path = entry
            .old_path
            .as_deref()
            .or(entry.new_path.as_deref())
            .ok_or_else(|| Error::InvalidRepository("diff entry has no path".into()))?;
        let new_path = entry.new_path.as_deref().unwrap_or(old_path);
        let mut output = format!(
            "diff --git {} {}\n",
            prefixed_path(b"a/", old_path),
            prefixed_path(b"b/", new_path)
        )
        .into_bytes();
        if entry.kind == DiffKind::Renamed {
            output.extend_from_slice(
                format!(
                    "similarity index 100%\nrename from {}\nrename to {}\n",
                    display_path(old_path),
                    display_path(new_path)
                )
                .as_bytes(),
            );
            return Ok(output);
        }
        match (entry.old_mode, entry.new_mode) {
            (None, Some(mode)) => {
                output.extend_from_slice(format!("new file mode {mode:06o}\n").as_bytes());
            }
            (Some(mode), None) => {
                output.extend_from_slice(format!("deleted file mode {mode:06o}\n").as_bytes());
            }
            (Some(old), Some(new)) if old != new => output
                .extend_from_slice(format!("old mode {old:06o}\nnew mode {new:06o}\n").as_bytes()),
            _ => {}
        }
        if entry.old_id != entry.new_id {
            let old = entry.old_id.unwrap_or_else(ObjectId::null);
            let new = entry.new_id.unwrap_or_else(ObjectId::null);
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode == new_mode
            {
                output.extend_from_slice(format!("index {old}..{new} {new_mode:06o}\n").as_bytes());
            } else {
                output.extend_from_slice(format!("index {old}..{new}\n").as_bytes());
            }
        }
        if entry.old_id == entry.new_id {
            return Ok(output);
        }
        let old_data = self.diff_blob(entry.old_id, entry.old_mode, options.max_object_size)?;
        let new_data = self.diff_blob(entry.new_id, entry.new_mode, options.max_object_size)?;
        let old_label = entry.old_id.map_or_else(
            || "/dev/null".to_owned(),
            |_| prefixed_path(b"a/", old_path),
        );
        let new_label = entry.new_id.map_or_else(
            || "/dev/null".to_owned(),
            |_| prefixed_path(b"b/", new_path),
        );
        output.extend_from_slice(format!("--- {old_label}\n+++ {new_label}\n").as_bytes());
        let (Some(old_data), Some(new_data)) = (old_data, new_data) else {
            output.extend_from_slice(
                format!("Binary files {old_label} and {new_label} differ\n").as_bytes(),
            );
            return Ok(output);
        };
        if old_data.contains(&0) || new_data.contains(&0) {
            output.extend_from_slice(
                format!("Binary files {old_label} and {new_label} differ\n").as_bytes(),
            );
            return Ok(output);
        }
        let old_lines = split_lines(&old_data);
        let new_lines = split_lines(&new_data);
        if old_lines.len().max(new_lines.len()) > options.max_lines {
            return Err(Error::InvalidRepository(format!(
                "diff exceeds {} lines",
                options.max_lines
            )));
        }
        let edits = myers(&old_lines, &new_lines, options.max_trace_cells)?;
        render_hunks(&mut output, &edits, options.context_lines);
        Ok(output)
    }

    fn optional_tree_map(
        &self,
        tree: Option<ObjectId>,
        max_size: usize,
    ) -> Result<BTreeMap<Vec<u8>, Value>> {
        tree.map(|tree| {
            self.flattened_tree(tree, max_size).map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| {
                        (
                            entry.path,
                            Value {
                                mode: entry.raw_mode,
                                id: entry.id,
                            },
                        )
                    })
                    .collect()
            })
        })
        .transpose()
        .map(Option::unwrap_or_default)
    }

    fn diff_blob(
        &self,
        id: Option<ObjectId>,
        mode: Option<u32>,
        max_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let Some(id) = id else {
            return Ok(Some(Vec::new()));
        };
        if mode == Some(0o160_000) {
            return Ok(None);
        }
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject(format!(
                "diff entry {id} is not a blob"
            )));
        }
        Ok(Some(object.data().to_vec()))
    }
}

fn diff_maps(
    old: &BTreeMap<Vec<u8>, Value>,
    new: &BTreeMap<Vec<u8>, Value>,
    detect_renames: bool,
) -> Vec<DiffEntry> {
    let paths = old
        .keys()
        .chain(new.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut entries = Vec::new();
    for path in paths {
        let before = old.get(&path).copied();
        let after = new.get(&path).copied();
        if before == after {
            continue;
        }
        entries.push(pair_entry(
            before.map(|value| (path.clone(), value)),
            after.map(|value| (path, value)),
        ));
    }
    if detect_renames {
        detect_exact_renames(&mut entries);
    }
    entries.sort_unstable_by(|left, right| {
        left.new_path
            .as_ref()
            .or(left.old_path.as_ref())
            .cmp(&right.new_path.as_ref().or(right.old_path.as_ref()))
    });
    entries
}

fn pair_entry(old: Option<(Vec<u8>, Value)>, new: Option<(Vec<u8>, Value)>) -> DiffEntry {
    let kind = match (&old, &new) {
        (None, Some(_)) => DiffKind::Added,
        (Some(_), None) => DiffKind::Deleted,
        (Some((_, old)), Some((_, new))) if mode_type(old.mode) != mode_type(new.mode) => {
            DiffKind::TypeChanged
        }
        (Some(_), Some(_)) => DiffKind::Modified,
        (None, None) => unreachable!("difference has an old or new side"),
    };
    let old_mode = old.as_ref().map(|(_, value)| value.mode);
    let new_mode = new.as_ref().map(|(_, value)| value.mode);
    let old_id = old.as_ref().map(|(_, value)| value.id);
    let new_id = new.as_ref().map(|(_, value)| value.id);
    DiffEntry {
        kind,
        old_path: old.map(|(path, _)| path),
        new_path: new.map(|(path, _)| path),
        old_mode,
        new_mode,
        old_id,
        new_id,
    }
}

fn detect_exact_renames(entries: &mut Vec<DiffEntry>) {
    let mut additions = BTreeMap::<(u32, ObjectId), Vec<usize>>::new();
    let mut deletions = BTreeMap::<(u32, ObjectId), Vec<usize>>::new();
    for (index, entry) in entries.iter().enumerate() {
        match entry.kind {
            DiffKind::Added => additions
                .entry((entry.new_mode.unwrap(), entry.new_id.unwrap()))
                .or_default()
                .push(index),
            DiffKind::Deleted => deletions
                .entry((entry.old_mode.unwrap(), entry.old_id.unwrap()))
                .or_default()
                .push(index),
            _ => {}
        }
    }
    let mut removed = BTreeSet::new();
    let mut renames = Vec::new();
    for (key, deleted) in deletions {
        let Some(added) = additions.get(&key) else {
            continue;
        };
        let mut candidates = deleted
            .iter()
            .flat_map(|deleted| {
                added.iter().map(|added| {
                    (
                        path_similarity(
                            entries[*deleted].old_path.as_deref().unwrap(),
                            entries[*added].new_path.as_deref().unwrap(),
                        ),
                        *deleted,
                        *added,
                    )
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|left, right| right.cmp(left));
        for (_, deleted, added) in candidates {
            if removed.contains(&deleted) || removed.contains(&added) {
                continue;
            }
            removed.insert(deleted);
            removed.insert(added);
            renames.push(DiffEntry {
                kind: DiffKind::Renamed,
                old_path: entries[deleted].old_path.clone(),
                new_path: entries[added].new_path.clone(),
                old_mode: entries[deleted].old_mode,
                new_mode: entries[added].new_mode,
                old_id: entries[deleted].old_id,
                new_id: entries[added].new_id,
            });
        }
    }
    let mut retained = entries
        .drain(..)
        .enumerate()
        .filter_map(|(index, entry)| (!removed.contains(&index)).then_some(entry))
        .collect::<Vec<_>>();
    retained.extend(renames);
    *entries = retained;
}

fn path_similarity(old: &[u8], new: &[u8]) -> usize {
    old.iter()
        .zip(new)
        .take_while(|(left, right)| left == right)
        .count()
        + old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take_while(|(left, right)| left == right)
            .count()
}

const fn mode_type(mode: u32) -> u32 {
    mode & 0o170_000
}

#[derive(Clone, Copy)]
enum Edit<'a> {
    Equal(&'a [u8]),
    Delete(&'a [u8]),
    Insert(&'a [u8]),
}

#[derive(Clone, Debug)]
struct LineChange<'a> {
    start: usize,
    end: usize,
    replacement: Vec<&'a [u8]>,
}

fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|byte| *byte == b'\n').collect()
}

pub(crate) fn merge_text(
    base_data: &[u8],
    ours_data: &[u8],
    theirs_data: &[u8],
    ours_label: &[u8],
    theirs_label: &[u8],
    max_lines: usize,
    max_trace_cells: usize,
) -> Result<(Vec<u8>, bool)> {
    let base = split_lines(base_data);
    let ours = split_lines(ours_data);
    let theirs = split_lines(theirs_data);
    if base.len().max(ours.len()).max(theirs.len()) > max_lines {
        return Err(Error::InvalidRepository(format!(
            "text merge exceeds {max_lines} lines"
        )));
    }
    let ours_changes = line_changes(&base, &ours, max_trace_cells)?;
    let theirs_changes = line_changes(&base, &theirs, max_trace_cells)?;
    let mut output = Vec::new();
    let mut base_position = 0usize;
    let mut ours_position = 0usize;
    let mut theirs_position = 0usize;
    let mut conflicted = false;
    while ours_position < ours_changes.len() || theirs_position < theirs_changes.len() {
        let ours_change = ours_changes.get(ours_position);
        let theirs_change = theirs_changes.get(theirs_position);
        match (ours_change, theirs_change) {
            (Some(ours_change), Some(theirs_change))
                if changes_are_disjoint(ours_change, theirs_change) =>
            {
                let (change, position) = if ours_change.start <= theirs_change.start {
                    (ours_change, &mut ours_position)
                } else {
                    (theirs_change, &mut theirs_position)
                };
                append_change(&mut output, &base, &mut base_position, change);
                *position += 1;
            }
            (Some(_), Some(_)) => {
                let start = ours_changes[ours_position]
                    .start
                    .min(theirs_changes[theirs_position].start);
                let mut end = ours_changes[ours_position]
                    .end
                    .max(theirs_changes[theirs_position].end);
                let mut ours_end = ours_position;
                let mut theirs_end = theirs_position;
                loop {
                    let previous = (ours_end, theirs_end, end);
                    ours_end = collect_overlapping(&ours_changes, ours_end, start, &mut end);
                    theirs_end = collect_overlapping(&theirs_changes, theirs_end, start, &mut end);
                    if previous == (ours_end, theirs_end, end) {
                        break;
                    }
                }
                let ours_region =
                    apply_changes(&base, start, end, &ours_changes[ours_position..ours_end]);
                let theirs_region = apply_changes(
                    &base,
                    start,
                    end,
                    &theirs_changes[theirs_position..theirs_end],
                );
                output.extend(base[base_position..start].iter().copied().flatten());
                if ours_region == theirs_region {
                    output.extend_from_slice(&ours_region);
                } else {
                    conflicted = true;
                    output.extend_from_slice(b"<<<<<<< ");
                    output.extend_from_slice(ours_label);
                    output.push(b'\n');
                    append_with_terminal_newline(&mut output, &ours_region);
                    output.extend_from_slice(b"=======\n");
                    append_with_terminal_newline(&mut output, &theirs_region);
                    output.extend_from_slice(b">>>>>>> ");
                    output.extend_from_slice(theirs_label);
                    output.push(b'\n');
                }
                base_position = end;
                ours_position = ours_end;
                theirs_position = theirs_end;
            }
            (Some(change), None) => {
                append_change(&mut output, &base, &mut base_position, change);
                ours_position += 1;
            }
            (None, Some(change)) => {
                append_change(&mut output, &base, &mut base_position, change);
                theirs_position += 1;
            }
            (None, None) => break,
        }
    }
    output.extend(base[base_position..].iter().copied().flatten());
    Ok((output, conflicted))
}

fn line_changes<'a>(
    base: &[&'a [u8]],
    side: &[&'a [u8]],
    max_trace_cells: usize,
) -> Result<Vec<LineChange<'a>>> {
    let mut changes = Vec::new();
    let mut base_position = 0usize;
    let mut pending: Option<LineChange<'a>> = None;
    for edit in myers(base, side, max_trace_cells)? {
        match edit {
            Edit::Equal(_) => {
                if let Some(change) = pending.take() {
                    changes.push(change);
                }
                base_position += 1;
            }
            Edit::Delete(_) => {
                let change = pending.get_or_insert_with(|| LineChange {
                    start: base_position,
                    end: base_position,
                    replacement: Vec::new(),
                });
                base_position += 1;
                change.end = base_position;
            }
            Edit::Insert(line) => pending
                .get_or_insert_with(|| LineChange {
                    start: base_position,
                    end: base_position,
                    replacement: Vec::new(),
                })
                .replacement
                .push(line),
        }
    }
    if let Some(change) = pending {
        changes.push(change);
    }
    Ok(changes)
}

fn changes_are_disjoint(left: &LineChange<'_>, right: &LineChange<'_>) -> bool {
    if left.start == left.end && right.start == right.end && left.start == right.start {
        return false;
    }
    left.end <= right.start || right.end <= left.start
}

fn collect_overlapping(
    changes: &[LineChange<'_>],
    start_index: usize,
    region_start: usize,
    region_end: &mut usize,
) -> usize {
    let mut index = start_index;
    while let Some(change) = changes.get(index) {
        let overlaps = change.start < *region_end
            || change.start == region_start
            || *region_end == region_start && change.start == *region_end;
        if !overlaps {
            break;
        }
        *region_end = (*region_end).max(change.end);
        index += 1;
    }
    index
}

fn apply_changes(base: &[&[u8]], start: usize, end: usize, changes: &[LineChange<'_>]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut position = start;
    for change in changes {
        output.extend(base[position..change.start].iter().copied().flatten());
        output.extend(change.replacement.iter().copied().flatten());
        position = change.end;
    }
    output.extend(base[position..end].iter().copied().flatten());
    output
}

fn append_change(
    output: &mut Vec<u8>,
    base: &[&[u8]],
    base_position: &mut usize,
    change: &LineChange<'_>,
) {
    output.extend(base[*base_position..change.start].iter().copied().flatten());
    output.extend(change.replacement.iter().copied().flatten());
    *base_position = change.end;
}

fn append_with_terminal_newline(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(value);
    if !value.ends_with(b"\n") {
        output.push(b'\n');
    }
}

pub(crate) fn unchanged_line_map(
    old_data: &[u8],
    new_data: &[u8],
    max_lines: usize,
    max_trace_cells: usize,
) -> Result<Vec<Option<usize>>> {
    let old = split_lines(old_data);
    let new = split_lines(new_data);
    if old.len().max(new.len()) > max_lines {
        return Err(Error::InvalidRepository(format!(
            "diff exceeds {max_lines} lines"
        )));
    }
    let edits = myers(&old, &new, max_trace_cells)?;
    let mut old_line = 0;
    let mut new_line = 0;
    let mut mapping = vec![None; new.len()];
    for edit in edits {
        match edit {
            Edit::Equal(_) => {
                mapping[new_line] = Some(old_line);
                old_line += 1;
                new_line += 1;
            }
            Edit::Delete(_) => old_line += 1,
            Edit::Insert(_) => new_line += 1,
        }
    }
    Ok(mapping)
}

#[allow(clippy::many_single_char_names)]
fn myers<'a>(old: &[&'a [u8]], new: &[&'a [u8]], max_cells: usize) -> Result<Vec<Edit<'a>>> {
    let n = isize::try_from(old.len())
        .map_err(|_| Error::InvalidRepository("diff is too large".into()))?;
    let m = isize::try_from(new.len())
        .map_err(|_| Error::InvalidRepository("diff is too large".into()))?;
    let max = usize::try_from(n + m)
        .map_err(|_| Error::InvalidRepository("diff size overflow".into()))?;
    if max == 0 {
        return Ok(Vec::new());
    }
    let width = max
        .checked_mul(2)
        .and_then(|value| value.checked_add(3))
        .ok_or_else(|| Error::InvalidRepository("diff trace overflow".into()))?;
    let offset = isize::try_from(max + 1)
        .map_err(|_| Error::InvalidRepository("diff offset overflow".into()))?;
    let mut v = vec![0isize; width];
    let mut trace = Vec::new();
    for distance in 0..=max {
        if (distance + 1)
            .checked_mul(width)
            .is_none_or(|cells| cells > max_cells)
        {
            return Err(Error::InvalidRepository(format!(
                "diff trace exceeds {max_cells} cells"
            )));
        }
        let d = isize::try_from(distance).expect("distance bounded by isize conversion above");
        for k in (-d..=d).step_by(2) {
            let index = usize::try_from(offset + k).expect("offset covers edit diagonal");
            let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
                v[index + 1]
            } else {
                v[index - 1] + 1
            };
            let mut y = x - k;
            while x < n
                && y < m
                && old[usize::try_from(x).expect("x is nonnegative")]
                    == new[usize::try_from(y).expect("y is nonnegative")]
            {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n && y >= m {
                trace.push(v.clone());
                return Ok(backtrack(old, new, &trace, distance, offset));
            }
        }
        trace.push(v.clone());
    }
    unreachable!("edit graph always reaches its opposite corner")
}

fn backtrack<'a>(
    old: &[&'a [u8]],
    new: &[&'a [u8]],
    trace: &[Vec<isize>],
    distance: usize,
    offset: isize,
) -> Vec<Edit<'a>> {
    let mut x = isize::try_from(old.len()).expect("length validated by myers");
    let mut y = isize::try_from(new.len()).expect("length validated by myers");
    let mut edits = Vec::new();
    for d_usize in (1..=distance).rev() {
        let d = isize::try_from(d_usize).expect("distance validated by myers");
        let previous = &trace[d_usize - 1];
        let k = x - y;
        let index = usize::try_from(offset + k).expect("offset covers backtrack diagonal");
        let previous_k = if k == -d || (k != d && previous[index - 1] < previous[index + 1]) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = previous[usize::try_from(offset + previous_k).unwrap()];
        let previous_y = previous_x - previous_k;
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            edits.push(Edit::Equal(
                old[usize::try_from(x).expect("x is nonnegative")],
            ));
        }
        if x == previous_x {
            y -= 1;
            edits.push(Edit::Insert(
                new[usize::try_from(y).expect("y is nonnegative")],
            ));
        } else {
            x -= 1;
            edits.push(Edit::Delete(
                old[usize::try_from(x).expect("x is nonnegative")],
            ));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        edits.push(Edit::Equal(
            old[usize::try_from(x).expect("x is nonnegative")],
        ));
    }
    while x > 0 {
        x -= 1;
        edits.push(Edit::Delete(
            old[usize::try_from(x).expect("x is nonnegative")],
        ));
    }
    while y > 0 {
        y -= 1;
        edits.push(Edit::Insert(
            new[usize::try_from(y).expect("y is nonnegative")],
        ));
    }
    edits.reverse();
    edits
}

fn render_hunks(output: &mut Vec<u8>, edits: &[Edit<'_>], context: usize) {
    let changes = edits
        .iter()
        .enumerate()
        .filter_map(|(index, edit)| (!matches!(edit, Edit::Equal(_))).then_some(index))
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return;
    }
    let mut groups = Vec::new();
    let mut start = changes[0];
    let mut end = changes[0];
    for change in changes.into_iter().skip(1) {
        if change.saturating_sub(end) > context.saturating_mul(2).saturating_add(1) {
            groups.push((
                start.saturating_sub(context),
                (end + context + 1).min(edits.len()),
            ));
            start = change;
        }
        end = change;
    }
    groups.push((
        start.saturating_sub(context),
        (end + context + 1).min(edits.len()),
    ));
    for (start, end) in groups {
        let old_start = 1 + edits[..start]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Insert(_)))
            .count();
        let new_start = 1 + edits[..start]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Delete(_)))
            .count();
        let old_count = edits[start..end]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Insert(_)))
            .count();
        let new_count = edits[start..end]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Delete(_)))
            .count();
        let old_start = if old_count == 0 {
            old_start.saturating_sub(1)
        } else {
            old_start
        };
        let new_start = if new_count == 0 {
            new_start.saturating_sub(1)
        } else {
            new_start
        };
        output.extend_from_slice(
            format!(
                "@@ -{} +{} @@\n",
                format_range(old_start, old_count),
                format_range(new_start, new_count)
            )
            .as_bytes(),
        );
        for edit in &edits[start..end] {
            let (prefix, line) = match edit {
                Edit::Equal(line) => (b' ', *line),
                Edit::Delete(line) => (b'-', *line),
                Edit::Insert(line) => (b'+', *line),
            };
            output.push(prefix);
            output.extend_from_slice(line);
            if !line.ends_with(b"\n") {
                output.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
}

fn format_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

fn display_path(path: &[u8]) -> String {
    quoted_path(&[], path)
}

fn prefixed_path(prefix: &[u8], path: &[u8]) -> String {
    quoted_path(prefix, path)
}

fn quoted_path(prefix: &[u8], path: &[u8]) -> String {
    let safe = prefix
        .iter()
        .chain(path)
        .all(|byte| (0x21..=0x7e).contains(byte) && !matches!(byte, b'"' | b'\\'));
    if safe {
        return String::from_utf8(prefix.iter().chain(path).copied().collect())
            .expect("safe path bytes are ASCII");
    }
    let mut output = String::from("\"");
    for byte in prefix.iter().chain(path).copied() {
        match byte {
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            0x20..=0x7e => output.push(char::from(byte)),
            _ => write!(output, "\\{byte:03o}").expect("writing to a String cannot fail"),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::{DiffKind, DiffOptions, Edit, merge_text, myers, split_lines};
    use crate::{
        EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository, Tree, TreeEntry,
    };

    #[test]
    fn three_way_text_merge_combines_regions_and_marks_overlaps() {
        let (merged, conflicted) = merge_text(
            b"one\ntwo\nthree\n",
            b"ONE\ntwo\nthree\n",
            b"one\ntwo\nTHREE\n",
            b"ours",
            b"theirs",
            10,
            10_000,
        )
        .unwrap();
        assert!(!conflicted);
        assert_eq!(merged, b"ONE\ntwo\nTHREE\n");

        let (merged, conflicted) = merge_text(
            b"one\ntwo\n",
            b"one\nOURS\n",
            b"one\nTHEIRS\n",
            b"ours",
            b"theirs",
            10,
            10_000,
        )
        .unwrap();
        assert!(conflicted);
        assert_eq!(
            merged,
            b"one\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\n"
        );
        assert!(merge_text(b"a\nb\n", b"a\nb\n", b"a\nb\n", b"o", b"t", 1, 100).is_err());
    }

    #[test]
    fn classifies_add_delete_modify_type_and_exact_rename() {
        let repository = repository();
        let shared = repository
            .write_object(ObjectKind::Blob, b"shared\n")
            .unwrap();
        let old_modified = repository.write_object(ObjectKind::Blob, b"old\n").unwrap();
        let new_modified = repository.write_object(ObjectKind::Blob, b"new\n").unwrap();
        let old_tree = tree(
            &repository,
            &[
                (EntryMode::Blob, b"delete", shared),
                (EntryMode::Blob, b"modify", old_modified),
                (EntryMode::Blob, b"old-name", shared),
                (EntryMode::Blob, b"type", shared),
            ],
        );
        let new_tree = tree(
            &repository,
            &[
                (EntryMode::Blob, b"add", new_modified),
                (EntryMode::Blob, b"modify", new_modified),
                (EntryMode::Blob, b"new-name", shared),
                (EntryMode::Link, b"type", shared),
            ],
        );
        let entries = repository
            .diff_trees(Some(old_tree), Some(new_tree), &DiffOptions::default())
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.kind(), entry.new_path().or(entry.old_path()).unwrap()))
                .collect::<Vec<_>>(),
            [
                (DiffKind::Added, b"add".as_slice()),
                (DiffKind::Deleted, b"delete".as_slice()),
                (DiffKind::Modified, b"modify".as_slice()),
                (DiffKind::Renamed, b"new-name".as_slice()),
                (DiffKind::TypeChanged, b"type".as_slice()),
            ]
        );
    }

    #[test]
    fn renders_git_style_hunks_binary_markers_and_no_newline_state() {
        let repository = repository();
        let old_blob = repository
            .write_object(ObjectKind::Blob, b"one\ntwo\nthree")
            .unwrap();
        let new_blob = repository
            .write_object(ObjectKind::Blob, b"one\nchanged\nthree")
            .unwrap();
        let old_tree = tree(&repository, &[(EntryMode::Blob, b"file", old_blob)]);
        let new_tree = tree(&repository, &[(EntryMode::Blob, b"file", new_blob)]);
        let entry = repository
            .diff_trees(Some(old_tree), Some(new_tree), &DiffOptions::default())
            .unwrap()
            .remove(0);
        let patch = repository
            .render_patch(&entry, &DiffOptions::default())
            .unwrap();
        let patch = String::from_utf8(patch).unwrap();
        assert!(patch.contains("@@ -1,3 +1,3 @@\n one\n-two\n+changed\n three\n"));
        assert_eq!(patch.matches("\\ No newline at end of file").count(), 1);

        let binary = repository
            .write_object(ObjectKind::Blob, b"binary\0data")
            .unwrap();
        let binary_tree = tree(&repository, &[(EntryMode::Blob, b"file", binary)]);
        let binary_entry = repository
            .diff_trees(Some(new_tree), Some(binary_tree), &DiffOptions::default())
            .unwrap()
            .remove(0);
        assert!(
            String::from_utf8(
                repository
                    .render_patch(&binary_entry, &DiffOptions::default())
                    .unwrap()
            )
            .unwrap()
            .contains("Binary files a/file and b/file differ")
        );

        let odd_old = tree(&repository, &[(EntryMode::Blob, b"odd-\xff", old_blob)]);
        let odd_new = tree(&repository, &[(EntryMode::Blob, b"odd-\xff", new_blob)]);
        let odd = repository
            .diff_trees(Some(odd_old), Some(odd_new), &DiffOptions::default())
            .unwrap()
            .remove(0);
        let odd_patch = String::from_utf8(
            repository
                .render_patch(&odd, &DiffOptions::default())
                .unwrap(),
        )
        .unwrap();
        assert!(odd_patch.starts_with("diff --git \"a/odd-\\377\" \"b/odd-\\377\"\n"));
    }

    #[test]
    fn myers_edit_script_reconstructs_both_sides_for_small_sequences() {
        let alphabet = [b"a\n".as_slice(), b"b\n".as_slice(), b"c\n".as_slice()];
        let sequences = (0..40)
            .map(|mut value| {
                let length = value % 4;
                value /= 4;
                (0..length)
                    .map(|_| {
                        let item = alphabet[value % alphabet.len()];
                        value /= alphabet.len();
                        item
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for old in &sequences {
            for new in &sequences {
                let edits = myers(old, new, 100_000).unwrap();
                let reconstructed_old = edits
                    .iter()
                    .filter_map(|edit| match edit {
                        Edit::Equal(line) | Edit::Delete(line) => Some(*line),
                        Edit::Insert(_) => None,
                    })
                    .collect::<Vec<_>>();
                let reconstructed_new = edits
                    .iter()
                    .filter_map(|edit| match edit {
                        Edit::Equal(line) | Edit::Insert(line) => Some(*line),
                        Edit::Delete(_) => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(&reconstructed_old, old);
                assert_eq!(&reconstructed_new, new);
            }
        }
        assert_eq!(split_lines(b"a\nb").len(), 2);
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }

    fn tree(
        repository: &Repository,
        entries: &[(EntryMode, &[u8], crate::ObjectId)],
    ) -> crate::ObjectId {
        repository
            .write_tree(
                &Tree::new(
                    entries
                        .iter()
                        .map(|(mode, name, id)| TreeEntry::new(*mode, name.to_vec(), *id).unwrap())
                        .collect(),
                )
                .unwrap(),
            )
            .unwrap()
    }
}
