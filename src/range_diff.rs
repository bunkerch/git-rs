//! Bounded global correspondence between two versions of a patch series.

use std::collections::{HashMap, VecDeque};

use crate::{
    DiffEntry, DiffKind, DiffOptions, Error, GraphOptions, ObjectId, Repository, Result,
    RevisionWalkOptions, diff::diff_line_cost,
};

const INFINITE_COST: i64 = i64::MAX / 16;

/// Resource limits and pairing policy for [`Repository::range_diff`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeDiffOptions {
    pub graph: GraphOptions,
    pub diff: DiffOptions,
    pub creation_factor: u32,
    pub max_cost_matrix_bytes: usize,
    pub max_comparisons: usize,
    pub max_patch_bytes: usize,
}

impl Default for RangeDiffOptions {
    fn default() -> Self {
        Self {
            graph: GraphOptions::default(),
            diff: DiffOptions::default(),
            creation_factor: 60,
            max_cost_matrix_bytes: 1024 * 1024 * 1024,
            max_comparisons: 1_000_000,
            max_patch_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Relationship between an old and new patch-series position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeDiffStatus {
    Equal,
    Changed,
    Dropped,
    Added,
}

/// One row in new-series-first range-diff order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeDiffEntry {
    old_position: Option<usize>,
    new_position: Option<usize>,
    old_commit: Option<ObjectId>,
    new_commit: Option<ObjectId>,
    status: RangeDiffStatus,
    subject: Vec<u8>,
}

impl RangeDiffEntry {
    /// One-based position in the old series.
    #[must_use]
    pub const fn old_position(&self) -> Option<usize> {
        self.old_position
    }
    /// One-based position in the new series.
    #[must_use]
    pub const fn new_position(&self) -> Option<usize> {
        self.new_position
    }
    #[must_use]
    pub const fn old_commit(&self) -> Option<ObjectId> {
        self.old_commit
    }
    #[must_use]
    pub const fn new_commit(&self) -> Option<ObjectId> {
        self.new_commit
    }
    #[must_use]
    pub const fn status(&self) -> RangeDiffStatus {
        self.status
    }
    #[must_use]
    pub fn subject(&self) -> &[u8] {
        &self.subject
    }
}

#[derive(Clone)]
struct SeriesPatch {
    id: ObjectId,
    subject: Vec<u8>,
    full: Vec<u8>,
    diff: Vec<u8>,
    diff_lines: usize,
    matching: Option<usize>,
    shown: bool,
}

impl Repository {
    /// Compare two base-exclusive, tip-inclusive patch series globally.
    ///
    /// Merge commits are excluded. Exact canonical diffs are paired first;
    /// remaining patches use a global minimum-cost line-difference assignment
    /// versus the configured add/drop creation cost.
    ///
    /// # Errors
    /// Returns an error for malformed history or objects, diff/graph limits,
    /// arithmetic overflow, or cost-matrix/comparison/patch-byte limits.
    pub fn range_diff(
        &self,
        old_base: ObjectId,
        old_tip: ObjectId,
        new_base: ObjectId,
        new_tip: ObjectId,
        options: &RangeDiffOptions,
    ) -> Result<Vec<RangeDiffEntry>> {
        if options.creation_factor == 0 {
            return Err(Error::InvalidRepository(
                "range-diff creation factor must be positive".into(),
            ));
        }
        let mut patch_bytes = 0usize;
        let mut old = self.series(old_base, old_tip, options, &mut patch_bytes)?;
        let mut new = self.series(new_base, new_tip, options, &mut patch_bytes)?;
        find_exact_matches(&mut old, &mut new);
        assign_correspondences(&mut old, &mut new, options)?;
        Ok(ordered_entries(&mut old, &new))
    }

    fn series(
        &self,
        base: ObjectId,
        tip: ObjectId,
        options: &RangeDiffOptions,
        patch_bytes: &mut usize,
    ) -> Result<Vec<SeriesPatch>> {
        let mut revisions = self.walk_revisions(
            &[tip],
            &[base],
            &RevisionWalkOptions {
                graph: options.graph.clone(),
                max_count: None,
                first_parent: false,
            },
        )?;
        revisions.reverse();
        revisions
            .into_iter()
            .filter(|revision| revision.commit().parents().len() <= 1)
            .map(|revision| {
                let commit = revision.commit();
                let old_tree = commit
                    .parents()
                    .first()
                    .map(|parent| self.read_commit(*parent, options.diff.max_object_size))
                    .transpose()?
                    .map(|parent| parent.tree());
                let changes = self.diff_trees(old_tree, Some(commit.tree()), &options.diff)?;
                let diff = self.canonical_range_diff(&changes, &options.diff)?;
                let full = canonical_full(
                    commit.author().name(),
                    commit.author().email(),
                    commit.message(),
                    &diff,
                );
                *patch_bytes = patch_bytes.checked_add(full.len()).ok_or_else(|| {
                    Error::InvalidRepository("range-diff patch size overflow".into())
                })?;
                if *patch_bytes > options.max_patch_bytes {
                    return Err(Error::InvalidRepository(format!(
                        "range-diff patches exceed {} bytes",
                        options.max_patch_bytes
                    )));
                }
                Ok(SeriesPatch {
                    id: revision.id(),
                    subject: commit_subject(commit.message()),
                    diff_lines: newline_count(&diff),
                    full,
                    diff,
                    matching: None,
                    shown: false,
                })
            })
            .collect()
    }

    fn canonical_range_diff(
        &self,
        changes: &[DiffEntry],
        options: &DiffOptions,
    ) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for change in changes {
            let old_path = change
                .old_path()
                .or(change.new_path())
                .ok_or_else(|| Error::InvalidRepository("range-diff entry has no path".into()))?;
            let new_path = change.new_path().unwrap_or(old_path);
            output.extend_from_slice(b"\n ## ");
            match change.kind() {
                DiffKind::Added => {
                    output.extend_from_slice(new_path);
                    output.extend_from_slice(b" (new)");
                }
                DiffKind::Deleted => {
                    output.extend_from_slice(old_path);
                    output.extend_from_slice(b" (deleted)");
                }
                DiffKind::Renamed => {
                    output.extend_from_slice(old_path);
                    output.extend_from_slice(b" => ");
                    output.extend_from_slice(new_path);
                }
                DiffKind::Modified | DiffKind::TypeChanged => output.extend_from_slice(new_path),
            }
            if let (Some(old), Some(new)) = (change.old_mode(), change.new_mode())
                && old != new
            {
                output
                    .extend_from_slice(format!(" (mode change {old:06o} => {new:06o})").as_bytes());
            }
            output.extend_from_slice(b" ##\n");
            append_canonical_hunks(&mut output, &self.render_patch(change, options)?, new_path);
        }
        Ok(output)
    }
}

fn canonical_full(name: &str, email: &str, message: &[u8], diff: &[u8]) -> Vec<u8> {
    let mut output =
        format!(" ## Metadata ##\nAuthor: {name} <{email}>\n\n ## Commit message ##\n")
            .into_bytes();
    for line in message.split(|byte| *byte == b'\n') {
        output.extend_from_slice(b"    ");
        output.extend_from_slice(trim_ascii_end(line));
        output.push(b'\n');
    }
    output.extend_from_slice(diff);
    output
}

fn append_canonical_hunks(output: &mut Vec<u8>, rendered_patch: &[u8], path: &[u8]) {
    let mut in_content = false;
    for line in rendered_patch.split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(b"@@ ") {
            in_content = true;
            output.extend_from_slice(b"@@");
            if let Some(end) = body[3..]
                .windows(2)
                .position(|window| window == b"@@")
                .map(|position| position + 3)
            {
                let suffix = &body[end + 2..];
                if !suffix.is_empty() {
                    output.push(b' ');
                    output.extend_from_slice(path);
                    output.push(b':');
                    output.extend_from_slice(suffix);
                }
            }
            output.push(b'\n');
        } else if in_content {
            output.extend_from_slice(line);
        } else if body.starts_with(b"Binary files ") {
            output.push(b' ');
            output.extend_from_slice(line);
        }
    }
}

fn newline_count(value: &[u8]) -> usize {
    value
        .iter()
        .fold(0usize, |count, byte| count + usize::from(*byte == b'\n'))
}

fn find_exact_matches(old: &mut [SeriesPatch], new: &mut [SeriesPatch]) {
    let mut available = HashMap::<Vec<u8>, VecDeque<usize>>::new();
    for (index, patch) in old.iter().enumerate() {
        available
            .entry(patch.diff.clone())
            .or_default()
            .push_back(index);
    }
    for (new_index, patch) in new.iter_mut().enumerate() {
        let Some(old_index) = available.get_mut(&patch.diff).and_then(VecDeque::pop_front) else {
            continue;
        };
        old[old_index].matching = Some(new_index);
        patch.matching = Some(old_index);
    }
}

fn assign_correspondences(
    old: &mut [SeriesPatch],
    new: &mut [SeriesPatch],
    options: &RangeDiffOptions,
) -> Result<()> {
    let comparisons = old
        .len()
        .checked_mul(new.len())
        .ok_or_else(|| Error::InvalidRepository("range-diff comparison count overflow".into()))?;
    if comparisons > options.max_comparisons {
        return Err(Error::InvalidRepository(format!(
            "range-diff exceeds {} comparisons",
            options.max_comparisons
        )));
    }
    let size = old
        .len()
        .checked_add(new.len())
        .ok_or_else(|| Error::InvalidRepository("range-diff matrix dimension overflow".into()))?;
    if size == 0 {
        return Ok(());
    }
    let cells = size
        .checked_mul(size)
        .ok_or_else(|| Error::InvalidRepository("range-diff matrix size overflow".into()))?;
    let bytes = cells
        .checked_mul(std::mem::size_of::<i64>())
        .ok_or_else(|| Error::InvalidRepository("range-diff matrix byte size overflow".into()))?;
    if bytes >= options.max_cost_matrix_bytes {
        return Err(Error::InvalidRepository(format!(
            "range-diff cost matrix requires {bytes} bytes, limited to {}",
            options.max_cost_matrix_bytes
        )));
    }
    let mut costs = vec![0_i64; cells];
    for (new_index, new_patch) in new.iter().enumerate() {
        for (old_index, old_patch) in old.iter().enumerate() {
            costs[new_index * size + old_index] = if old_patch.matching == Some(new_index) {
                0
            } else if old_patch.matching.is_none() && new_patch.matching.is_none() {
                i64::try_from(diff_line_cost(
                    &old_patch.diff,
                    &new_patch.diff,
                    options.diff.max_lines,
                    options.diff.max_trace_cells,
                )?)
                .map_err(|_| Error::InvalidRepository("range-diff cost overflow".into()))?
            } else {
                INFINITE_COST
            };
        }
        let creation = creation_cost(new_patch.diff_lines, options.creation_factor)?;
        for old_index in old.len()..size {
            costs[new_index * size + old_index] = if new_patch.matching.is_none() {
                creation
            } else {
                INFINITE_COST
            };
        }
    }
    for (old_index, old_patch) in old.iter().enumerate() {
        let creation = creation_cost(old_patch.diff_lines, options.creation_factor)?;
        for row in new.len()..size {
            costs[row * size + old_index] = if old_patch.matching.is_none() {
                creation
            } else {
                INFINITE_COST
            };
        }
    }
    for (new_index, old_index) in minimum_assignment(size, &costs)?
        .into_iter()
        .enumerate()
        .take(new.len())
    {
        if old_index < old.len() {
            old[old_index].matching = Some(new_index);
            new[new_index].matching = Some(old_index);
        }
    }
    Ok(())
}

fn creation_cost(lines: usize, factor: u32) -> Result<i64> {
    let scaled = lines
        .checked_mul(factor as usize)
        .ok_or_else(|| Error::InvalidRepository("range-diff creation cost overflow".into()))?
        / 100;
    i64::try_from(scaled)
        .map_err(|_| Error::InvalidRepository("range-diff creation cost overflow".into()))
}

fn minimum_assignment(size: usize, costs: &[i64]) -> Result<Vec<usize>> {
    let mut row_potential = vec![0_i64; size + 1];
    let mut column_potential = vec![0_i64; size + 1];
    let mut column_row = vec![0usize; size + 1];
    let mut path = vec![0usize; size + 1];
    for row in 1..=size {
        column_row[0] = row;
        let mut column = 0usize;
        let mut minimum = vec![INFINITE_COST; size + 1];
        let mut used = vec![false; size + 1];
        loop {
            used[column] = true;
            let current_row = column_row[column];
            let mut delta = INFINITE_COST;
            let mut next_column = 0usize;
            for candidate in 1..=size {
                if used[candidate] {
                    continue;
                }
                let reduced = costs[(current_row - 1) * size + candidate - 1]
                    - row_potential[current_row]
                    - column_potential[candidate];
                if reduced < minimum[candidate] {
                    minimum[candidate] = reduced;
                    path[candidate] = column;
                }
                if minimum[candidate] < delta {
                    delta = minimum[candidate];
                    next_column = candidate;
                }
            }
            if delta == INFINITE_COST {
                return Err(Error::InvalidRepository(
                    "range-diff assignment has no finite solution".into(),
                ));
            }
            for candidate in 0..=size {
                if used[candidate] {
                    row_potential[column_row[candidate]] += delta;
                    column_potential[candidate] -= delta;
                } else {
                    minimum[candidate] -= delta;
                }
            }
            column = next_column;
            if column_row[column] == 0 {
                break;
            }
        }
        loop {
            let previous = path[column];
            column_row[column] = column_row[previous];
            column = previous;
            if column == 0 {
                break;
            }
        }
    }
    let mut row_to_column = vec![0usize; size];
    for column in 1..=size {
        row_to_column[column_row[column] - 1] = column - 1;
    }
    Ok(row_to_column)
}

fn ordered_entries(old: &mut [SeriesPatch], new: &[SeriesPatch]) -> Vec<RangeDiffEntry> {
    let (mut old_index, mut new_index) = (0usize, 0usize);
    let mut output = Vec::new();
    while old_index < old.len() || new_index < new.len() {
        while old_index < old.len() && old[old_index].shown {
            old_index += 1;
        }
        if old_index < old.len() && old[old_index].matching.is_none() {
            output.push(entry(Some((old_index, &old[old_index])), None));
            old_index += 1;
            continue;
        }
        while new_index < new.len() && new[new_index].matching.is_none() {
            output.push(entry(None, Some((new_index, &new[new_index]))));
            new_index += 1;
        }
        if new_index < new.len() {
            let matched = new[new_index]
                .matching
                .expect("matched new range-diff patch has an old position");
            output.push(entry(
                Some((matched, &old[matched])),
                Some((new_index, &new[new_index])),
            ));
            old[matched].shown = true;
            new_index += 1;
        }
    }
    output
}

fn entry(old: Option<(usize, &SeriesPatch)>, new: Option<(usize, &SeriesPatch)>) -> RangeDiffEntry {
    let status = match (old, new) {
        (Some((_, old)), Some((_, new))) if old.full == new.full => RangeDiffStatus::Equal,
        (Some(_), Some(_)) => RangeDiffStatus::Changed,
        (Some(_), None) => RangeDiffStatus::Dropped,
        (None, Some(_)) => RangeDiffStatus::Added,
        (None, None) => unreachable!("range-diff row has at least one side"),
    };
    let subject = old
        .map(|(_, patch)| patch.subject.clone())
        .or_else(|| new.map(|(_, patch)| patch.subject.clone()))
        .expect("range-diff row has a subject");
    RangeDiffEntry {
        old_position: old.map(|(index, _)| index + 1),
        new_position: new.map(|(index, _)| index + 1),
        old_commit: old.map(|(_, patch)| patch.id),
        new_commit: new.map(|(_, patch)| patch.id),
        status,
        subject,
    }
}

fn commit_subject(message: &[u8]) -> Vec<u8> {
    message
        .split(|byte| *byte == b'\n')
        .map(trim_ascii)
        .find(|line| !line.is_empty())
        .unwrap_or(b"<none>")
        .to_vec()
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    trim_ascii_end(value)
}

fn trim_ascii_end(mut value: &[u8]) -> &[u8] {
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{RangeDiffOptions, RangeDiffStatus, minimum_assignment};
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature,
        Tree, TreeEntry,
    };

    #[test]
    fn assignment_finds_global_minimum() {
        assert_eq!(
            minimum_assignment(3, &[9, 2, 7, 6, 4, 3, 5, 8, 1]).unwrap(),
            [1, 0, 2]
        );
    }

    #[test]
    fn pairs_reordered_changed_dropped_and_added_patches() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let base = commit(&repository, None, &[(b"base", b"base\n")], b"base", 1);
        let old_one = commit(
            &repository,
            Some(base),
            &[(b"one", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n")],
            b"one",
            2,
        );
        let old_two = commit(&repository, Some(old_one), &[(b"two", b"two\n")], b"two", 3);
        let old_drop = commit(
            &repository,
            Some(old_two),
            &[(b"drop", b"drop\n")],
            b"drop",
            4,
        );
        let new_two = commit(&repository, Some(base), &[(b"two", b"two\n")], b"two", 5);
        let new_one = commit(
            &repository,
            Some(new_two),
            &[(b"one", b"1\n2\n3\n4\nchanged\n6\n7\n8\n9\n10\n")],
            b"one v2",
            6,
        );
        let new_add = commit(&repository, Some(new_one), &[(b"add", b"add\n")], b"add", 7);
        let result = repository
            .range_diff(base, old_drop, base, new_add, &RangeDiffOptions::default())
            .unwrap();
        assert_eq!(
            result
                .iter()
                .map(|entry| (entry.old_position(), entry.new_position(), entry.status()))
                .collect::<Vec<_>>(),
            [
                (Some(2), Some(1), RangeDiffStatus::Equal),
                (Some(1), Some(2), RangeDiffStatus::Changed),
                (Some(3), None, RangeDiffStatus::Dropped),
                (None, Some(3), RangeDiffStatus::Added),
            ]
        );
        assert!(
            repository
                .range_diff(
                    base,
                    old_drop,
                    base,
                    new_add,
                    &RangeDiffOptions {
                        max_comparisons: 1,
                        ..RangeDiffOptions::default()
                    },
                )
                .is_err()
        );
    }

    fn commit(
        repository: &Repository,
        parent: Option<crate::ObjectId>,
        additions: &[(&[u8], &[u8])],
        message: &[u8],
        timestamp: i64,
    ) -> crate::ObjectId {
        let mut entries = parent
            .map(|id| repository.read_commit(id, 4096).unwrap().tree())
            .map(|tree| repository.read_tree(tree, 4096).unwrap().entries().to_vec())
            .unwrap_or_default();
        for (name, data) in additions {
            let blob = repository.write_object(ObjectKind::Blob, data).unwrap();
            entries.retain(|entry| entry.name() != *name);
            entries.push(TreeEntry::new(EntryMode::Blob, name.to_vec(), blob).unwrap());
        }
        let tree = repository.write_tree(&Tree::new(entries).unwrap()).unwrap();
        let identity = Signature::new("A", "a@example.com", timestamp, 0).unwrap();
        let mut builder =
            CommitBuilder::new(tree, identity.clone(), identity).message(message.to_vec());
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
