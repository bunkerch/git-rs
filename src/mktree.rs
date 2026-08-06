//! Build canonical tree objects from non-recursive `ls-tree` records.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::str::FromStr;

use crate::{EntryMode, Error, ObjectId, ObjectKind, Repository, Result};

struct MkEntry {
    raw_mode: u32,
    mode: EntryMode,
    name: Vec<u8>,
    id: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MkTreeOptions {
    /// Records are NUL terminated and names are used literally.
    pub nul_terminated: bool,
    /// Permit missing objects. Existing objects must still have the right type.
    pub missing: bool,
    /// Empty records delimit multiple trees.
    pub batch: bool,
    pub max_input_bytes: usize,
    pub max_entries_per_tree: usize,
    pub max_object_size: usize,
}

impl Default for MkTreeOptions {
    fn default() -> Self {
        Self {
            nul_terminated: false,
            missing: false,
            batch: false,
            max_input_bytes: 1024 * 1024 * 1024,
            max_entries_per_tree: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Parse non-recursive `ls-tree` records and write one or more tree objects.
    ///
    /// Non-batch mode returns exactly one ID, including the canonical empty
    /// tree for empty input. Batch mode returns one ID per delimited tree and
    /// does not synthesize a final empty tree at end-of-input.
    ///
    /// # Errors
    /// Returns an error for malformed or oversized input, invalid paths,
    /// duplicate entries, mode/type disagreement, missing objects unless
    /// allowed, wrong existing object types, or storage failure.
    pub fn mk_tree(&self, input: &[u8], options: &MkTreeOptions) -> Result<Vec<ObjectId>> {
        if input.len() > options.max_input_bytes {
            return mk_error("mktree input exceeds byte limit");
        }
        let terminator = if options.nul_terminated { 0 } else { b'\n' };
        let mut trees = Vec::new();
        let mut entries = Vec::new();
        let mut start = 0;
        while start < input.len() {
            let end = input[start..]
                .iter()
                .position(|byte| *byte == terminator)
                .map_or(input.len(), |offset| start + offset);
            let record = &input[start..end];
            start = end.saturating_add(1);
            if record.is_empty() {
                if !options.batch {
                    return mk_error("blank record is only valid in batch mode");
                }
                trees.push(self.write_mktree_entries(&mut entries)?);
                continue;
            }
            if entries.len() >= options.max_entries_per_tree {
                return mk_error("mktree entry count exceeds limit");
            }
            let entry = parse_record(record, options.nul_terminated)?;
            self.validate_mktree_object(&entry, options)?;
            entries.push(entry);
        }
        if !options.batch || !entries.is_empty() {
            trees.push(self.write_mktree_entries(&mut entries)?);
        }
        Ok(trees)
    }

    fn validate_mktree_object(&self, entry: &MkEntry, options: &MkTreeOptions) -> Result<()> {
        match self.read_object(entry.id, options.max_object_size) {
            Ok(object) if object.kind() == entry.mode.object_kind() => Ok(()),
            Ok(object) => mk_error(format!(
                "entry `{}` object is {}, expected {}",
                String::from_utf8_lossy(&entry.name),
                String::from_utf8_lossy(object.kind().as_bytes()),
                String::from_utf8_lossy(entry.mode.object_kind().as_bytes())
            )),
            Err(Error::NotFound(_)) if options.missing || entry.mode == EntryMode::Gitlink => {
                Ok(())
            }
            Err(Error::NotFound(_)) => mk_error(format!(
                "entry `{}` object {} is unavailable",
                String::from_utf8_lossy(&entry.name),
                entry.id
            )),
            Err(error) => Err(error),
        }
    }

    fn write_mktree_entries(&self, entries: &mut Vec<MkEntry>) -> Result<ObjectId> {
        let mut names = BTreeSet::new();
        if entries
            .iter()
            .any(|entry| !names.insert(entry.name.clone()))
        {
            return mk_error("duplicate mktree filename");
        }
        entries.sort_unstable_by(compare_entries);
        let capacity = entries
            .iter()
            .map(|entry| 7 + 1 + entry.name.len() + 1 + ObjectId::LENGTH)
            .sum();
        let mut body = Vec::with_capacity(capacity);
        for entry in entries.iter() {
            body.extend_from_slice(format!("{:o}", entry.raw_mode).as_bytes());
            body.push(b' ');
            body.extend_from_slice(&entry.name);
            body.push(0);
            body.extend_from_slice(entry.id.as_bytes());
        }
        entries.clear();
        self.write_object(ObjectKind::Tree, &body)
    }
}

fn parse_record(record: &[u8], nul_terminated: bool) -> Result<MkEntry> {
    let mode_end = record
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| invalid_record(record))?;
    let (raw_mode, mode) = parse_mode(&record[..mode_end])?;
    let type_start = mode_end + 1;
    let type_end = record[type_start..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|offset| type_start + offset)
        .ok_or_else(|| invalid_record(record))?;
    let declared_kind = parse_kind(&record[type_start..type_end])?;
    if declared_kind != mode.object_kind() {
        return mk_error("declared object type does not match mode");
    }
    let id_start = type_end + 1;
    let tab = record[id_start..]
        .iter()
        .position(|byte| *byte == b'\t')
        .map(|offset| id_start + offset)
        .ok_or_else(|| invalid_record(record))?;
    let id_text =
        std::str::from_utf8(&record[id_start..tab]).map_err(|_| invalid_record(record))?;
    let id = ObjectId::from_str(id_text).map_err(|_| invalid_record(record))?;
    let encoded_name = &record[tab + 1..];
    let name = if !nul_terminated && encoded_name.first() == Some(&b'"') {
        unquote_name(encoded_name)?
    } else {
        encoded_name.to_vec()
    };
    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
        return mk_error("mktree filename is empty or contains `/` or NUL");
    }
    Ok(MkEntry {
        raw_mode,
        mode,
        name,
        id,
    })
}

fn parse_mode(value: &[u8]) -> Result<(u32, EntryMode)> {
    let raw = std::str::from_utf8(value)
        .ok()
        .and_then(|value| u32::from_str_radix(value, 8).ok())
        .ok_or_else(|| invalid_record(value))?;
    let mode = match raw & 0o170_000 {
        0o100_000 if raw & 0o111 == 0 => Ok(EntryMode::Blob),
        0o100_000 => Ok(EntryMode::BlobExecutable),
        0o120_000 => Ok(EntryMode::Link),
        0o040_000 => Ok(EntryMode::Tree),
        0o160_000 => Ok(EntryMode::Gitlink),
        _ => mk_error(format!("unsupported mktree mode {raw:o}")),
    }?;
    Ok((raw, mode))
}

fn compare_entries(left: &MkEntry, right: &MkEntry) -> Ordering {
    let common = left.name.len().min(right.name.len());
    match left.name[..common].cmp(&right.name[..common]) {
        Ordering::Equal => {
            let left_next =
                left.name
                    .get(common)
                    .copied()
                    .unwrap_or(if left.mode == EntryMode::Tree {
                        b'/'
                    } else {
                        0
                    });
            let right_next =
                right
                    .name
                    .get(common)
                    .copied()
                    .unwrap_or(if right.mode == EntryMode::Tree {
                        b'/'
                    } else {
                        0
                    });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

fn parse_kind(value: &[u8]) -> Result<ObjectKind> {
    match value {
        b"blob" => Ok(ObjectKind::Blob),
        b"tree" => Ok(ObjectKind::Tree),
        b"commit" => Ok(ObjectKind::Commit),
        _ => mk_error("invalid mktree object type"),
    }
}

fn unquote_name(value: &[u8]) -> Result<Vec<u8>> {
    if value.first() != Some(&b'"') {
        return mk_error("invalid quoted mktree name");
    }
    let mut output = Vec::with_capacity(value.len().saturating_sub(2));
    let mut cursor = 1;
    while cursor < value.len() {
        let byte = value[cursor];
        cursor += 1;
        if byte == b'"' {
            return Ok(output);
        }
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        let escaped = *value
            .get(cursor)
            .ok_or_else(|| Error::InvalidTree("truncated quoted mktree name".into()))?;
        cursor += 1;
        match escaped {
            b'a' => output.push(0x07),
            b'b' => output.push(0x08),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(0x0b),
            b'f' => output.push(0x0c),
            b'r' => output.push(b'\r'),
            b'"' | b'\\' => output.push(escaped),
            b'0'..=b'3' => {
                let mut octal = u16::from(escaped - b'0');
                for _ in 0..2 {
                    let next @ b'0'..=b'7' = value
                        .get(cursor)
                        .copied()
                        .ok_or_else(|| Error::InvalidTree("truncated octal escape".into()))?
                    else {
                        return mk_error("invalid quoted mktree octal escape");
                    };
                    octal = octal * 8 + u16::from(next - b'0');
                    cursor += 1;
                }
                output.push(u8::try_from(octal).expect("three octal digits beginning below four"));
            }
            _ => return mk_error("invalid quoted mktree escape"),
        }
    }
    mk_error("unterminated quoted mktree name")
}

fn invalid_record(record: &[u8]) -> Error {
    Error::InvalidTree(format!(
        "invalid mktree record `{}`",
        String::from_utf8_lossy(record)
    ))
}

fn mk_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidTree(message.into()))
}

#[cfg(test)]
mod tests {
    use super::MkTreeOptions;
    use crate::{InitOptions, MemoryFileSystem, ObjectKind, Repository};

    #[test]
    fn sorts_records_and_decodes_newline_quoted_names() {
        let repository = repository();
        let a = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let b = repository.write_object(ObjectKind::Blob, b"b").unwrap();
        let input = format!("100644 blob {b}\tzeta\n100644 blob {a}\t\"a\\tname\"\n");
        let id = repository
            .mk_tree(input.as_bytes(), &MkTreeOptions::default())
            .unwrap()[0];
        let tree = repository.read_tree(id, 1024).unwrap();
        assert_eq!(tree.entries()[0].name(), b"a\tname");
        assert_eq!(tree.entries()[1].name(), b"zeta");
    }

    #[test]
    fn accepts_literal_names_and_multiple_nul_delimited_batches() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let input = format!("100644 blob {blob}\t\"literal\"\0\0100644 blob {blob}\tsecond\0");
        let ids = repository
            .mk_tree(
                input.as_bytes(),
                &MkTreeOptions {
                    nul_terminated: true,
                    batch: true,
                    ..MkTreeOptions::default()
                },
            )
            .unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            repository.read_tree(ids[0], 1024).unwrap().entries()[0].name(),
            b"\"literal\""
        );
        assert_eq!(
            repository.read_tree(ids[1], 1024).unwrap().entries()[0].name(),
            b"second"
        );
    }

    #[test]
    fn enforces_object_types_missing_policy_and_limits() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"a").unwrap();
        let missing = "1111111111111111111111111111111111111111";
        assert!(
            repository
                .mk_tree(
                    format!("040000 tree {blob}\twrong\n").as_bytes(),
                    &MkTreeOptions::default()
                )
                .is_err()
        );
        assert!(
            repository
                .mk_tree(
                    format!("100644 blob {missing}\tmissing\n").as_bytes(),
                    &MkTreeOptions::default()
                )
                .is_err()
        );
        assert!(
            repository
                .mk_tree(
                    format!("100644 blob {missing}\tmissing\n").as_bytes(),
                    &MkTreeOptions {
                        missing: true,
                        ..MkTreeOptions::default()
                    }
                )
                .is_ok()
        );
        assert!(
            repository
                .mk_tree(
                    format!("160000 commit {missing}\tsub\n").as_bytes(),
                    &MkTreeOptions::default()
                )
                .is_ok()
        );
        assert!(
            repository
                .mk_tree(
                    format!("100644 blob {blob}\ta\n").as_bytes(),
                    &MkTreeOptions {
                        max_entries_per_tree: 0,
                        ..MkTreeOptions::default()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn preserves_noncanonical_regular_permissions_like_native_git() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        let id = repository
            .mk_tree(
                format!("100664 blob {blob}\ta\n").as_bytes(),
                &MkTreeOptions::default(),
            )
            .unwrap()[0];
        assert_eq!(id.to_string(), "42fb942a880903fd2450d0dff0572d7ff2165855");
        assert_eq!(
            repository.read_tree(id, 1024).unwrap().entries()[0].mode(),
            crate::EntryMode::Blob
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
