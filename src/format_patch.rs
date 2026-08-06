//! Bounded Git-compatible email patch-series generation.

use std::fmt::Write as _;

use crate::{DiffOptions, Error, ObjectId, Repository, Result, RevisionWalkOptions, Signature};

/// Subject numbering policy for generated patch series.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FormatPatchNumbering {
    Never,
    #[default]
    Auto,
    Always,
}

/// Revision, rendering, naming, and allocation policy for format-patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatPatchOptions {
    pub walk: RevisionWalkOptions,
    pub diff: DiffOptions,
    pub numbering: FormatPatchNumbering,
    pub subject_prefix: String,
    pub reroll_count: Option<u32>,
    /// Text below Git's `-- ` signature delimiter. `None` omits the delimiter.
    pub signature: Option<Vec<u8>>,
    pub filename_max_bytes: usize,
    pub max_subject_bytes: usize,
    pub max_message_bytes: usize,
    pub max_patch_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for FormatPatchOptions {
    fn default() -> Self {
        Self {
            walk: RevisionWalkOptions::default(),
            diff: DiffOptions::default(),
            numbering: FormatPatchNumbering::Auto,
            subject_prefix: "PATCH".into(),
            reroll_count: None,
            signature: Some(b"git-rs".to_vec()),
            filename_max_bytes: 64,
            max_subject_bytes: 16 * 1024,
            max_message_bytes: 16 * 1024 * 1024,
            max_patch_bytes: 1024 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// One generated mail message and its suggested filesystem-independent name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatPatch {
    id: ObjectId,
    filename: String,
    data: Vec<u8>,
}

impl FormatPatch {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

impl Repository {
    /// Format non-merge commits reachable from `include` but not `exclude` as email patches.
    ///
    /// Output is topological oldest-first, suitable for feeding to `git am` or
    /// [`Repository::apply_patch`]. Merge commits are omitted because a single
    /// first-parent patch does not faithfully represent a merge.
    ///
    /// # Errors
    /// Returns an error for invalid options, graph/object/diff failures, invalid
    /// author dates, or any configured message, patch, or aggregate size limit.
    pub fn format_patches(
        &self,
        include: &[ObjectId],
        exclude: &[ObjectId],
        options: &FormatPatchOptions,
    ) -> Result<Vec<FormatPatch>> {
        validate_options(options)?;
        let output_limit = options.walk.max_count.unwrap_or(usize::MAX);
        let mut walk = options.walk.clone();
        walk.max_count = None;
        let mut revisions = self.walk_revisions(include, exclude, &walk)?;
        revisions.retain(|revision| revision.commit().parents().len() <= 1);
        revisions.truncate(output_limit);
        revisions.reverse();
        let count = revisions.len();
        let numbered = match options.numbering {
            FormatPatchNumbering::Never => false,
            FormatPatchNumbering::Auto => count > 1,
            FormatPatchNumbering::Always => true,
        };
        let width = count.max(1).to_string().len().max(4);
        let mut total = 0usize;
        let mut output = Vec::with_capacity(count);
        for (position, revision) in revisions.into_iter().enumerate() {
            let commit = revision.commit();
            if commit.message().len() > options.max_message_bytes {
                return Err(Error::ObjectTooLarge {
                    declared: commit.message().len() as u64,
                    limit: options.max_message_bytes,
                });
            }
            let (subject, body) = split_message(commit.message(), options.max_subject_bytes)?;
            let old_tree = commit
                .parents()
                .first()
                .map(|parent| self.read_commit(*parent, options.diff.max_object_size))
                .transpose()?
                .map(|parent| parent.tree());
            let changes = self.diff_trees(old_tree, Some(commit.tree()), &options.diff)?;
            let mut patch = Vec::new();
            for change in &changes {
                let rendered = self.render_patch(change, &options.diff)?;
                let next = patch
                    .len()
                    .checked_add(rendered.len())
                    .ok_or_else(|| Error::InvalidRepository("format-patch size overflow".into()))?;
                if next > options.max_patch_bytes {
                    return Err(Error::ObjectTooLarge {
                        declared: next as u64,
                        limit: options.max_patch_bytes,
                    });
                }
                patch.extend_from_slice(&rendered);
            }
            let number = position + 1;
            let data = render_mail(
                revision.id(),
                commit.author(),
                &subject,
                body,
                &patch,
                number,
                count,
                numbered,
                options,
            )?;
            total = total
                .checked_add(data.len())
                .ok_or_else(|| Error::InvalidRepository("patch series size overflow".into()))?;
            if total > options.max_total_bytes {
                return Err(Error::ObjectTooLarge {
                    declared: total as u64,
                    limit: options.max_total_bytes,
                });
            }
            let filename = format_filename(
                number,
                width,
                &subject,
                options.reroll_count,
                options.filename_max_bytes,
            )?;
            output.push(FormatPatch {
                id: revision.id(),
                filename,
                data,
            });
        }
        Ok(output)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_mail(
    id: ObjectId,
    author: &Signature,
    subject: &[u8],
    body: &[u8],
    patch: &[u8],
    number: usize,
    count: usize,
    numbered: bool,
    options: &FormatPatchOptions,
) -> Result<Vec<u8>> {
    let mut output = format!("From {id} Mon Sep 17 00:00:00 2001\nFrom: ").into_bytes();
    output.extend_from_slice(&encode_display_name(author.name()));
    output.extend_from_slice(
        format!(
            " <{}>\nDate: {}\nSubject: ",
            author.email(),
            rfc2822(author)?
        )
        .as_bytes(),
    );
    let mut prefix = options.subject_prefix.clone();
    if let Some(version) = options.reroll_count {
        write!(prefix, " v{version}").expect("writing to String cannot fail");
    }
    if numbered {
        write!(prefix, " {number}/{count}").expect("writing to String cannot fail");
    }
    output.extend_from_slice(format!("[{prefix}] ").as_bytes());
    output.extend_from_slice(&encode_subject(subject));
    output.extend_from_slice(b"\n\n");
    if !body.is_empty() {
        output.extend_from_slice(body);
        if !body.ends_with(b"\n") {
            output.push(b'\n');
        }
        output.push(b'\n');
    }
    output.extend_from_slice(b"---\n");
    output.extend_from_slice(patch);
    if !patch.ends_with(b"\n") {
        output.push(b'\n');
    }
    if let Some(signature) = &options.signature {
        output.extend_from_slice(b"-- \n");
        output.extend_from_slice(signature);
        if !signature.ends_with(b"\n") {
            output.push(b'\n');
        }
    }
    Ok(output)
}

fn split_message(message: &[u8], max_subject: usize) -> Result<(Vec<u8>, &[u8])> {
    let mut cursor = 0usize;
    let mut subject = Vec::new();
    let mut body_start = message.len();
    while cursor < message.len() {
        let relative_end = message[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(message.len() - cursor);
        let end = cursor + relative_end;
        let line = trim_ascii(&message[cursor..end]);
        let next = end.saturating_add(usize::from(end < message.len()));
        if line.is_empty() {
            body_start = next;
            break;
        }
        if !subject.is_empty() {
            subject.push(b' ');
        }
        subject.extend_from_slice(line);
        cursor = next;
    }
    if subject.len() > max_subject {
        return Err(Error::ObjectTooLarge {
            declared: subject.len() as u64,
            limit: max_subject,
        });
    }
    let subject = if subject.is_empty() {
        b"(no subject)".to_vec()
    } else {
        subject
    };
    let body = message.get(body_start..).unwrap_or_default();
    Ok((subject, trim_blank_lines(body)))
}

fn encode_subject(subject: &[u8]) -> Vec<u8> {
    if subject
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte) && !matches!(*byte, b'\r' | b'\n'))
    {
        return subject.to_vec();
    }
    format!("=?UTF-8?B?{}?=", base64(subject)).into_bytes()
}

fn encode_display_name(name: &str) -> Vec<u8> {
    if !name.is_ascii() {
        return encode_subject(name.as_bytes());
    }
    if name.bytes().any(|byte| {
        matches!(
            byte,
            b'(' | b')'
                | b'<'
                | b'>'
                | b'@'
                | b','
                | b';'
                | b':'
                | b'\\'
                | b'"'
                | b'.'
                | b'['
                | b']'
        )
    }) {
        let mut output = Vec::with_capacity(name.len() + 2);
        output.push(b'"');
        for byte in name.bytes() {
            if matches!(byte, b'"' | b'\\') {
                output.push(b'\\');
            }
            output.push(byte);
        }
        output.push(b'"');
        output
    } else {
        name.as_bytes().to_vec()
    }
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(char::from(ALPHABET[((value >> 18) & 63) as usize]));
        output.push(char::from(ALPHABET[((value >> 12) & 63) as usize]));
        output.push(if chunk.len() > 1 {
            char::from(ALPHABET[((value >> 6) & 63) as usize])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(ALPHABET[(value & 63) as usize])
        } else {
            '='
        });
    }
    output
}

fn format_filename(
    number: usize,
    width: usize,
    subject: &[u8],
    version: Option<u32>,
    max_bytes: usize,
) -> Result<String> {
    let mut slug = String::new();
    let mut pending_dash = false;
    for &byte in subject {
        if byte.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(char::from(byte));
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("patch");
    }
    let prefix = version.map_or_else(
        || format!("{number:0width$}-"),
        |version| format!("v{version}-{number:0width$}-"),
    );
    let suffix = ".patch";
    let overhead = prefix
        .len()
        .checked_add(suffix.len())
        .ok_or_else(|| Error::InvalidRepository("patch filename length overflow".into()))?;
    if overhead >= max_bytes {
        return Err(Error::InvalidRepository(
            "filename limit cannot hold patch numbering".into(),
        ));
    }
    let available = max_bytes - overhead;
    slug.truncate(available.min(slug.len()));
    while slug.ends_with('-') {
        slug.pop();
    }
    Ok(format!("{prefix}{slug}{suffix}"))
}

fn rfc2822(signature: &Signature) -> Result<String> {
    let local = signature
        .timestamp()
        .checked_add(i64::from(signature.offset_minutes()) * 60)
        .ok_or_else(|| Error::InvalidCommit("author date overflows local time".into()))?;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(Error::InvalidCommit(
            "author year is outside RFC 2822 range".into(),
        ));
    }
    let weekdays = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let offset = signature.offset_minutes();
    let sign = if offset < 0 || signature.has_unknown_timezone() {
        '-'
    } else {
        '+'
    };
    let absolute = offset.unsigned_abs();
    Ok(format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} {sign}{:02}{:02}",
        weekdays[usize::try_from(days.rem_euclid(7)).expect("weekday is in 0..7")],
        months[month as usize - 1],
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        absolute / 60,
        absolute % 60,
    ))
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u32::try_from(month).expect("civil month is positive and bounded"),
        u32::try_from(day).expect("civil day is positive and bounded"),
    )
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_blank_lines(mut value: &[u8]) -> &[u8] {
    while value.starts_with(b"\n") {
        value = &value[1..];
    }
    while value.ends_with(b"\n\n") {
        value = &value[..value.len() - 1];
    }
    value
}

fn validate_options(options: &FormatPatchOptions) -> Result<()> {
    if options.subject_prefix.is_empty()
        || options
            .subject_prefix
            .contains(['\r', '\n', '\0', '[', ']'])
        || options.filename_max_bytes < 12
    {
        return Err(Error::InvalidRepository(
            "invalid format-patch options".into(),
        ));
    }
    if options
        .signature
        .as_ref()
        .is_some_and(|value| value.contains(&0))
    {
        return Err(Error::InvalidRepository(
            "format-patch signature contains NUL".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        ApplyOptions, CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem,
        ObjectKind, Tree, TreeEntry,
    };

    #[test]
    fn emits_oldest_first_numbered_series_that_can_be_applied() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"one\n", b"Add file\n\nFirst body.\n", 0);
        let tip = commit(&repository, &[root], b"two\n", b"Change file\n", 60);
        let patches = repository
            .format_patches(&[tip], &[], &FormatPatchOptions::default())
            .unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].id(), root);
        assert_eq!(patches[1].id(), tip);
        assert_eq!(patches[0].filename(), "0001-Add-file.patch");
        assert!(
            patches[0]
                .data()
                .starts_with(format!("From {root} ").as_bytes())
        );
        let first = String::from_utf8_lossy(patches[0].data());
        assert!(first.contains("Date: Thu, 01 Jan 1970 00:00:00 +0000\n"));
        assert!(first.contains("Subject: [PATCH 1/2] Add file\n"));
        assert!(first.contains("\nFirst body.\n\n---\n"));
        repository
            .apply_patch(patches[0].data(), &ApplyOptions::default())
            .unwrap();
        repository
            .apply_patch(patches[1].data(), &ApplyOptions::default())
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"two\n");
    }

    #[test]
    fn skips_merges_before_output_limit_and_honors_reroll_policy() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"root\n", b"Root\n", 1);
        let first = commit(&repository, &[root], b"first\n", b"First\n", 2);
        let side = commit(&repository, &[root], b"side\n", b"Side\n", 3);
        let merge = commit(&repository, &[first, side], b"merged\n", b"Merge\n", 4);
        let patches = repository
            .format_patches(
                &[merge],
                &[root],
                &FormatPatchOptions {
                    numbering: FormatPatchNumbering::Always,
                    reroll_count: Some(3),
                    walk: RevisionWalkOptions {
                        max_count: Some(1),
                        ..RevisionWalkOptions::default()
                    },
                    ..FormatPatchOptions::default()
                },
            )
            .unwrap();
        assert_eq!(patches.len(), 1);
        assert_ne!(patches[0].id(), merge);
        assert!(patches[0].filename().starts_with("v3-0001-"));
        assert!(String::from_utf8_lossy(patches[0].data()).contains("Subject: [PATCH v3 1/1]"));
    }

    #[test]
    fn encodes_non_ascii_subjects_and_enforces_aggregate_limit() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], b"value\n", "Résumé ✓\n".as_bytes(), 0);
        let patch = repository
            .format_patches(
                &[root],
                &[],
                &FormatPatchOptions {
                    signature: None,
                    ..FormatPatchOptions::default()
                },
            )
            .unwrap()
            .pop()
            .unwrap();
        assert!(String::from_utf8_lossy(patch.data()).contains("Subject: [PATCH] =?UTF-8?B?"));
        assert!(!patch.data().ends_with(b"git-rs\n"));
        assert!(
            repository
                .format_patches(
                    &[root],
                    &[],
                    &FormatPatchOptions {
                        max_total_bytes: 1,
                        ..FormatPatchOptions::default()
                    },
                )
                .is_err()
        );
    }

    fn commit(
        repository: &Repository,
        parents: &[ObjectId],
        contents: &[u8],
        message: &[u8],
        timestamp: i64,
    ) -> ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let author = Signature::new("Patch Author", "author@example.com", timestamp, 0).unwrap();
        let committer =
            Signature::new("Patch Committer", "committer@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, author, committer).message(message.to_vec());
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
