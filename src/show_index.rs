//! Bounded inspection of Git pack index versions 1 and 2.

use std::path::Path;

use crate::{Error, ObjectId, PackIndex, Repository, Result};

const HASH_SIZE: usize = ObjectId::LENGTH;
const FANOUT_BYTES: usize = 256 * 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShowIndexOptions {
    pub max_index_size: usize,
    pub max_objects: usize,
}

impl Default for ShowIndexOptions {
    fn default() -> Self {
        Self {
            max_index_size: 1024 * 1024 * 1024,
            max_objects: 10_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShowIndexEntry {
    pub offset: u64,
    pub id: ObjectId,
    pub crc32: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowIndexReport {
    pub version: u32,
    pub entries: Vec<ShowIndexEntry>,
    pub pack_checksum: [u8; HASH_SIZE],
}

/// Parse a complete SHA-1 pack index without opening its sibling pack.
///
/// # Errors
/// Returns an error for oversized input, excessive objects, unsupported
/// versions, malformed tables, checksum failure, ordering, or offset aliases.
pub fn show_index(data: &[u8], options: &ShowIndexOptions) -> Result<ShowIndexReport> {
    if data.len() > options.max_index_size {
        return Err(Error::ObjectTooLarge {
            declared: data.len() as u64,
            limit: options.max_index_size,
        });
    }
    if data.len() < FANOUT_BYTES + 2 * HASH_SIZE {
        return invalid("truncated pack index");
    }
    let version = if data.starts_with(&[0xff, b't', b'O', b'c']) {
        if read_u32(data, 4)? != 2 {
            return invalid("unsupported pack index version");
        }
        2
    } else {
        1
    };
    let fanout_start = if version == 2 { 8 } else { 0 };
    let count = validate_fanout(data, fanout_start, options.max_objects)?;
    if version == 2 {
        let index = PackIndex::parse(data)?;
        if index.entries().len() != count {
            return invalid("pack index count disagreement");
        }
        return Ok(ShowIndexReport {
            version,
            entries: index
                .entries()
                .iter()
                .map(|entry| ShowIndexEntry {
                    offset: entry.offset,
                    id: entry.id,
                    crc32: Some(entry.crc32),
                })
                .collect(),
            pack_checksum: *index.pack_checksum(),
        });
    }
    parse_v1(data, count)
}

impl Repository {
    /// Read and inspect one pack index through the configured filesystem.
    ///
    /// # Errors
    /// Returns path/storage errors or the validation errors from [`show_index`].
    pub fn show_index(
        &self,
        path: impl AsRef<Path>,
        options: &ShowIndexOptions,
    ) -> Result<ShowIndexReport> {
        let path = crate::fs::normalize_path(path.as_ref())?;
        show_index(&self.filesystem().read(&path)?, options)
    }
}

fn parse_v1(data: &[u8], count: usize) -> Result<ShowIndexReport> {
    let entry_bytes = count
        .checked_mul(4 + HASH_SIZE)
        .ok_or_else(|| Error::InvalidObject("pack index size overflow".into()))?;
    let expected = FANOUT_BYTES
        .checked_add(entry_bytes)
        .and_then(|size| size.checked_add(2 * HASH_SIZE))
        .ok_or_else(|| Error::InvalidObject("pack index size overflow".into()))?;
    if data.len() != expected {
        return invalid("version-1 pack index has trailing or truncated data");
    }
    let checksum_start = data.len() - HASH_SIZE;
    if crate::object::sha1::digest(&data[..checksum_start]) != data[checksum_start..] {
        return invalid("pack index checksum mismatch");
    }
    let pack_checksum_start = checksum_start - HASH_SIZE;
    let mut pack_checksum = [0; HASH_SIZE];
    pack_checksum.copy_from_slice(&data[pack_checksum_start..checksum_start]);
    let mut entries = Vec::with_capacity(count);
    let mut previous = None;
    let mut offsets = std::collections::BTreeSet::new();
    for index in 0..count {
        let start = FANOUT_BYTES + index * (4 + HASH_SIZE);
        let offset = u64::from(read_u32(data, start)?);
        if !offsets.insert(offset) {
            return invalid("multiple objects have the same pack offset");
        }
        let mut bytes = [0; HASH_SIZE];
        bytes.copy_from_slice(&data[start + 4..start + 4 + HASH_SIZE]);
        let id = ObjectId::from_bytes(bytes);
        if previous.is_some_and(|old| old >= id) {
            return invalid("object IDs are not strictly sorted");
        }
        previous = Some(id);
        entries.push(ShowIndexEntry {
            offset,
            id,
            crc32: None,
        });
    }
    validate_fanout_ids(data, 0, &entries)?;
    Ok(ShowIndexReport {
        version: 1,
        entries,
        pack_checksum,
    })
}

fn validate_fanout(data: &[u8], start: usize, max_objects: usize) -> Result<usize> {
    let mut previous = 0_u32;
    for bucket in 0..256 {
        let count = read_u32(data, start + bucket * 4)?;
        if count < previous {
            return invalid("non-monotonic pack index fanout");
        }
        previous = count;
    }
    let count = usize::try_from(previous)
        .map_err(|_| Error::InvalidObject("pack index count overflow".into()))?;
    if count > max_objects {
        return Err(Error::InvalidRepository(
            "show-index object limit exceeded".into(),
        ));
    }
    Ok(count)
}

fn validate_fanout_ids(data: &[u8], start: usize, entries: &[ShowIndexEntry]) -> Result<()> {
    let mut buckets = [0_u32; 256];
    for entry in entries {
        let bucket = usize::from(entry.id.as_bytes()[0]);
        buckets[bucket] = buckets[bucket]
            .checked_add(1)
            .ok_or_else(|| Error::InvalidObject("pack index count overflow".into()))?;
    }
    let mut cumulative = 0_u32;
    for (bucket, count) in buckets.into_iter().enumerate() {
        cumulative = cumulative
            .checked_add(count)
            .ok_or_else(|| Error::InvalidObject("pack index count overflow".into()))?;
        if read_u32(data, start + bucket * 4)? != cumulative {
            return invalid("fanout does not match object IDs");
        }
    }
    Ok(())
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| Error::InvalidObject("truncated pack index integer".into()))
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::InvalidObject(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InitOptions, MemoryFileSystem, ObjectKind, PackOptions};

    #[test]
    fn inspects_version_two_offsets_crcs_and_adapter_paths() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let ids = vec![
            repository.write_object(ObjectKind::Blob, b"a").unwrap(),
            repository.write_object(ObjectKind::Blob, b"b").unwrap(),
        ];
        let written = repository
            .write_pack(&ids, &PackOptions::default())
            .unwrap();
        let report = repository
            .show_index(&written.index_path, &ShowIndexOptions::default())
            .unwrap();
        assert_eq!(report.version, 2);
        assert_eq!(report.entries.len(), 2);
        assert!(
            report
                .entries
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        assert!(report.entries.iter().all(|entry| entry.crc32.is_some()));
        assert_eq!(report.pack_checksum, written.checksum);
    }

    #[test]
    fn parses_version_one_without_crc_and_validates_invariants() {
        let first = ObjectId::from_bytes([0x11; ObjectId::LENGTH]);
        let second = ObjectId::from_bytes([0x22; ObjectId::LENGTH]);
        let data = v1(&[(12, first), (1234, second)]);
        let report = show_index(&data, &ShowIndexOptions::default()).unwrap();
        assert_eq!(report.version, 1);
        assert_eq!(
            report.entries,
            vec![
                ShowIndexEntry {
                    offset: 12,
                    id: first,
                    crc32: None
                },
                ShowIndexEntry {
                    offset: 1234,
                    id: second,
                    crc32: None
                },
            ]
        );
        assert_eq!(report.pack_checksum, [0x55; HASH_SIZE]);

        let mut corrupt = data.clone();
        corrupt[100] ^= 1;
        assert!(show_index(&corrupt, &ShowIndexOptions::default()).is_err());
        assert!(
            show_index(
                &data,
                &ShowIndexOptions {
                    max_objects: 1,
                    ..ShowIndexOptions::default()
                }
            )
            .is_err()
        );
        assert!(
            show_index(
                &data,
                &ShowIndexOptions {
                    max_index_size: data.len() - 1,
                    ..ShowIndexOptions::default()
                }
            )
            .is_err()
        );
    }

    fn v1(entries: &[(u32, ObjectId)]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut count = 0_u32;
        for bucket in 0..256 {
            count += u32::try_from(
                entries
                    .iter()
                    .filter(|(_, id)| usize::from(id.as_bytes()[0]) == bucket)
                    .count(),
            )
            .unwrap();
            data.extend_from_slice(&count.to_be_bytes());
        }
        for (offset, id) in entries {
            data.extend_from_slice(&offset.to_be_bytes());
            data.extend_from_slice(id.as_bytes());
        }
        data.extend_from_slice(&[0x55; HASH_SIZE]);
        let checksum = crate::object::sha1::digest(&data);
        data.extend_from_slice(&checksum);
        data
    }
}
