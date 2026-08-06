//! Standalone three-way text-file and blob merging.

use crate::{Error, ObjectId, ObjectKind, Repository, Result};

/// Conflict marker presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MergeFileStyle {
    #[default]
    Merge,
    Diff3,
    ZDiff3,
}

/// Automatic conflict resolution policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MergeFileFavor {
    #[default]
    Normal,
    Ours,
    Theirs,
    Union,
}

/// Labels, presentation, and resource bounds for a file merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeFileOptions {
    pub current_label: Vec<u8>,
    pub base_label: Vec<u8>,
    pub other_label: Vec<u8>,
    pub style: MergeFileStyle,
    pub favor: MergeFileFavor,
    pub marker_size: usize,
    pub max_input_size: usize,
    pub max_output_size: usize,
    pub max_lines: usize,
    pub max_trace_cells: usize,
    /// Store the merged bytes when using [`Repository::merge_blobs`].
    pub write_object: bool,
}

impl Default for MergeFileOptions {
    fn default() -> Self {
        Self {
            current_label: b"current".to_vec(),
            base_label: b"base".to_vec(),
            other_label: b"other".to_vec(),
            style: MergeFileStyle::Merge,
            favor: MergeFileFavor::Normal,
            marker_size: 7,
            max_input_size: 1024 * 1024 * 1024,
            max_output_size: 4 * 1024 * 1024 * 1024,
            max_lines: 10_000_000,
            max_trace_cells: 100_000_000,
            write_object: false,
        }
    }
}

/// Merged bytes, conflict count, and an optional stored blob ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeFileResult {
    data: Vec<u8>,
    conflicts: usize,
    object_id: Option<ObjectId>,
}

impl MergeFileResult {
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    #[must_use]
    pub const fn conflicts(&self) -> usize {
        self.conflicts
    }
    #[must_use]
    pub const fn has_conflicts(&self) -> bool {
        self.conflicts != 0
    }
    #[must_use]
    pub const fn object_id(&self) -> Option<ObjectId> {
        self.object_id
    }
}

/// Merge `current` and `other` changes relative to `base`.
///
/// # Errors
/// Returns an error for binary input, invalid marker/limits, or an exceeded
/// input, output, line, or diff-trace bound.
pub fn merge_file(
    current: &[u8],
    base: &[u8],
    other: &[u8],
    options: &MergeFileOptions,
) -> Result<MergeFileResult> {
    validate_inputs(current, base, other, options)?;
    let (data, conflicts) = crate::diff::merge_text_configured(base, current, other, options)?;
    Ok(MergeFileResult {
        data,
        conflicts,
        object_id: None,
    })
}

impl Repository {
    /// Merge three blob objects, optionally storing the result as a blob.
    ///
    /// # Errors
    /// Returns an error for missing, oversized, corrupt, or non-blob objects,
    /// merge failures, or object-storage failures.
    pub fn merge_blobs(
        &self,
        current: ObjectId,
        base: ObjectId,
        other: ObjectId,
        options: &MergeFileOptions,
    ) -> Result<MergeFileResult> {
        let current = self.read_merge_blob(current, options.max_input_size)?;
        let base = self.read_merge_blob(base, options.max_input_size)?;
        let other = self.read_merge_blob(other, options.max_input_size)?;
        let mut result = merge_file(&current, &base, &other, options)?;
        if options.write_object {
            result.object_id = Some(self.write_object(ObjectKind::Blob, &result.data)?);
        }
        Ok(result)
    }

    fn read_merge_blob(&self, id: ObjectId, max_size: usize) -> Result<Vec<u8>> {
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject(format!(
                "merge-file object {id} is not a blob"
            )));
        }
        Ok(object.into_data())
    }
}

fn validate_inputs(
    current: &[u8],
    base: &[u8],
    other: &[u8],
    options: &MergeFileOptions,
) -> Result<()> {
    if options.marker_size == 0 {
        return Err(Error::InvalidRepository(
            "merge-file marker size is zero".into(),
        ));
    }
    if options.marker_size > options.max_output_size
        || options
            .current_label
            .len()
            .max(options.base_label.len())
            .max(options.other_label.len())
            > options.max_output_size
    {
        return Err(Error::InvalidRepository(
            "merge-file marker or label exceeds output limit".into(),
        ));
    }
    if current.len().max(base.len()).max(other.len()) > options.max_input_size {
        return Err(Error::InvalidRepository(
            "merge-file input exceeds size limit".into(),
        ));
    }
    if [current, base, other]
        .iter()
        .any(|input| input.iter().take(8000).any(|byte| *byte == 0))
    {
        return Err(Error::InvalidRepository("cannot merge binary files".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InitOptions, MemoryFileSystem};

    const BASE: &[u8] = b"start\nbase\nend\n";
    const CURRENT: &[u8] = b"start\ncommon\ncurrent\nend\n";
    const OTHER: &[u8] = b"start\ncommon\nother\nend\n";

    #[test]
    fn renders_styles_labels_markers_and_favor_modes() {
        let normal = merge_file(
            CURRENT,
            BASE,
            OTHER,
            &MergeFileOptions {
                current_label: b"left".to_vec(),
                base_label: b"ancestor".to_vec(),
                other_label: b"right".to_vec(),
                marker_size: 5,
                ..MergeFileOptions::default()
            },
        )
        .unwrap();
        assert_eq!(normal.conflicts(), 1);
        assert!(
            normal
                .data()
                .windows(10)
                .any(|value| value == b"<<<<< left")
        );

        let diff3 = merge_file(
            CURRENT,
            BASE,
            OTHER,
            &MergeFileOptions {
                style: MergeFileStyle::Diff3,
                ..MergeFileOptions::default()
            },
        )
        .unwrap();
        assert!(
            diff3
                .data()
                .windows(12)
                .any(|value| value == b"||||||| base")
        );

        let zdiff3 = merge_file(
            CURRENT,
            BASE,
            OTHER,
            &MergeFileOptions {
                style: MergeFileStyle::ZDiff3,
                ..MergeFileOptions::default()
            },
        )
        .unwrap();
        assert!(zdiff3.data().starts_with(b"start\ncommon\n<<<<<<<"));

        for (favor, expected) in [
            (
                MergeFileFavor::Ours,
                b"start\ncommon\ncurrent\nend\n".as_slice(),
            ),
            (
                MergeFileFavor::Theirs,
                b"start\ncommon\nother\nend\n".as_slice(),
            ),
            (
                MergeFileFavor::Union,
                b"start\ncommon\ncurrent\nother\nend\n".as_slice(),
            ),
        ] {
            let result = merge_file(
                CURRENT,
                BASE,
                OTHER,
                &MergeFileOptions {
                    favor,
                    ..MergeFileOptions::default()
                },
            )
            .unwrap();
            assert_eq!(result.conflicts(), 0);
            assert_eq!(result.data(), expected);
        }
    }

    #[test]
    fn blob_mode_writes_result_and_rejects_binary_or_limits() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let current = repository.write_object(ObjectKind::Blob, CURRENT).unwrap();
        let base = repository.write_object(ObjectKind::Blob, BASE).unwrap();
        let other = repository.write_object(ObjectKind::Blob, OTHER).unwrap();
        let result = repository
            .merge_blobs(
                current,
                base,
                other,
                &MergeFileOptions {
                    favor: MergeFileFavor::Ours,
                    write_object: true,
                    ..MergeFileOptions::default()
                },
            )
            .unwrap();
        let stored = repository
            .read_object(result.object_id().unwrap(), 4096)
            .unwrap();
        assert_eq!(stored.data(), result.data());
        assert!(merge_file(b"a\0", b"a", b"b", &MergeFileOptions::default()).is_err());
        assert!(
            merge_file(
                b"a\n",
                b"a\n",
                b"b\n",
                &MergeFileOptions {
                    max_input_size: 1,
                    ..MergeFileOptions::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn crlf_conflict_markers_preserve_line_endings() {
        let result = merge_file(
            b"a\r\ncurrent\r\n",
            b"a\r\nbase\r\n",
            b"a\r\nother\r\n",
            &MergeFileOptions::default(),
        )
        .unwrap();
        assert_eq!(result.conflicts(), 1);
        assert!(
            !result
                .data()
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'\n'
                    && index.checked_sub(1).and_then(|i| result.data().get(i)) != Some(&b'\r'))
        );
    }
}
