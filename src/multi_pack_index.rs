//! Git-compatible multi-pack-index storage and lookup.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::object::sha1;
use crate::{Error, ObjectId, PackIndex, Repository, Result};

const PATH: &str = "objects/pack/multi-pack-index";
const LARGE_OFFSET: u32 = 0x8000_0000;
const CACHE_FILE_LIMIT: usize = 1024 * 1024 * 1024;

/// Resource limits and selection policy for MIDX generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiPackIndexOptions {
    pub max_packs: usize,
    pub max_objects: usize,
    pub max_pack_index_size: usize,
    pub max_file_size: usize,
    /// An `.idx` basename whose duplicate objects should be selected first.
    pub preferred_pack: Option<String>,
    pub force: bool,
    pub dry_run: bool,
}

impl Default for MultiPackIndexOptions {
    fn default() -> Self {
        Self {
            max_packs: 1_000_000,
            max_objects: 100_000_000,
            max_pack_index_size: 1024 * 1024 * 1024,
            max_file_size: 8 * 1024 * 1024 * 1024usize,
            preferred_pack: None,
            force: false,
            dry_run: false,
        }
    }
}

/// Generation result and inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultiPackIndexReport {
    pub packs: usize,
    pub objects: usize,
    pub duplicate_objects: usize,
    pub bytes: usize,
    pub changed: bool,
}

/// One unique object location in a MIDX.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultiPackIndexEntry {
    id: ObjectId,
    pack_index: u32,
    offset: u64,
}

impl MultiPackIndexEntry {
    #[must_use]
    pub const fn id(self) -> ObjectId {
        self.id
    }
    #[must_use]
    pub const fn pack_index(self) -> u32 {
        self.pack_index
    }
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// A validated non-incremental SHA-1 multi-pack-index version 1 or 2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiPackIndex {
    pack_names: Vec<String>,
    entries: Vec<MultiPackIndexEntry>,
}

impl MultiPackIndex {
    /// Parse and checksum a non-incremental MIDX v1 or v2.
    ///
    /// # Errors
    /// Returns an error for unsupported, malformed, corrupt, or over-limit data.
    pub fn parse(data: &[u8], max_packs: usize, max_objects: usize) -> Result<Self> {
        parse(data, max_packs, max_objects)
    }

    #[must_use]
    pub fn pack_names(&self) -> &[String] {
        &self.pack_names
    }
    #[must_use]
    pub fn entries(&self) -> &[MultiPackIndexEntry] {
        &self.entries
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[must_use]
    pub fn find(&self, id: ObjectId) -> Option<MultiPackIndexEntry> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
            .map(|position| self.entries[position])
    }
    #[must_use]
    pub fn pack_name(&self, index: u32) -> Option<&str> {
        self.pack_names.get(index as usize).map(String::as_str)
    }
}

impl Repository {
    /// Read and validate the repository's MIDX through its filesystem adapter.
    ///
    /// # Errors
    /// Returns an error for storage failures, malformed data, or exceeded limits.
    pub fn read_multi_pack_index(
        &self,
        max_file_size: usize,
        max_packs: usize,
        max_objects: usize,
    ) -> Result<MultiPackIndex> {
        let data = self.read_git_file(PATH)?;
        if data.len() > max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: data.len() as u64,
                limit: max_file_size,
            });
        }
        MultiPackIndex::parse(&data, max_packs, max_objects)
    }

    /// Index every valid `pack-*.idx` in `objects/pack`.
    ///
    /// Duplicate object IDs are represented once. An explicit preferred pack
    /// wins; otherwise the lexicographically first pack provides the location.
    ///
    /// # Errors
    /// Returns an error for invalid/missing packs or indexes, invalid policy,
    /// exceeded limits, lock contention, or storage failures.
    #[allow(clippy::too_many_lines)]
    pub fn write_multi_pack_index(
        &self,
        options: &MultiPackIndexOptions,
    ) -> Result<MultiPackIndexReport> {
        let directory = self.git_path("objects/pack");
        let mut names = Vec::new();
        for child in self.filesystem().read_dir(&directory)? {
            let Some(name) = child.to_str() else {
                return invalid("non-UTF-8 pack index name");
            };
            if !is_pack_index_name(name) {
                continue;
            }
            if !self
                .filesystem()
                .exists(&directory.join(child.with_extension("pack")))?
            {
                return invalid("pack index has no corresponding pack");
            }
            names.push(name.to_owned());
            if names.len() > options.max_packs {
                return invalid("pack count exceeds limit");
            }
        }
        names.sort_unstable();
        if names.is_empty() {
            return invalid("no pack files to index");
        }
        if let Some(preferred) = &options.preferred_pack
            && !names.iter().any(|name| name == preferred)
        {
            return invalid("preferred pack is not present");
        }
        let mut selected = HashMap::<ObjectId, MultiPackIndexEntry>::new();
        let mut total = 0usize;
        for (pack_position, name) in names.iter().enumerate() {
            let path = directory.join(name);
            let metadata = self.filesystem().metadata(&path)?;
            if usize::try_from(metadata.len()).unwrap_or(usize::MAX) > options.max_pack_index_size {
                return Err(Error::ObjectTooLarge {
                    declared: metadata.len(),
                    limit: options.max_pack_index_size,
                });
            }
            let index = PackIndex::parse(&self.filesystem().read(&path)?)?;
            total = total
                .checked_add(index.entries().len())
                .ok_or_else(|| midx_error("object count overflow"))?;
            if total > options.max_objects {
                return invalid("object count exceeds limit");
            }
            let preferred = options.preferred_pack.as_deref() == Some(name);
            for entry in index.entries() {
                let candidate = MultiPackIndexEntry {
                    id: entry.id,
                    pack_index: u32::try_from(pack_position)
                        .map_err(|_| midx_error("pack count overflow"))?,
                    offset: entry.offset,
                };
                match selected.entry(entry.id) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(candidate);
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) if preferred => {
                        slot.insert(candidate);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }
        let duplicate_objects = total - selected.len();
        let mut entries = selected.into_values().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.id);
        if entries.is_empty() {
            return invalid("pack files contain no objects");
        }
        let bytes = encode(&names, &entries)?;
        if bytes.len() > options.max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: bytes.len() as u64,
                limit: options.max_file_size,
            });
        }
        let old = match self.read_git_file(PATH) {
            Ok(data) => Some(data),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let changed = options.force || old.as_deref() != Some(bytes.as_slice());
        if changed && !options.dry_run {
            self.write_atomic(Path::new(PATH), &bytes)?;
            *self
                .multi_pack_index
                .write()
                .map_err(|_| midx_error("MIDX cache lock poisoned"))? =
                Some(Arc::new(MultiPackIndex {
                    pack_names: names.clone(),
                    entries: entries.clone(),
                }));
        }
        Ok(MultiPackIndexReport {
            packs: names.len(),
            objects: entries.len(),
            duplicate_objects,
            bytes: bytes.len(),
            changed,
        })
    }

    pub(crate) fn midx_object_location(&self, id: ObjectId) -> Result<Option<(PathBuf, u64)>> {
        let cached = self
            .multi_pack_index
            .read()
            .map_err(|_| midx_error("MIDX cache lock poisoned"))?
            .clone();
        let midx = if let Some(midx) = cached {
            midx
        } else {
            let metadata = match self.filesystem().metadata(&self.git_path(PATH)) {
                Ok(metadata) => metadata,
                Err(Error::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            if metadata.len() > u64::try_from(CACHE_FILE_LIMIT).expect("limit fits u64") {
                return Err(Error::ObjectTooLarge {
                    declared: metadata.len(),
                    limit: CACHE_FILE_LIMIT,
                });
            }
            let data = match self.read_git_file(PATH) {
                Ok(data) => data,
                Err(Error::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let parsed = Arc::new(MultiPackIndex::parse(&data, 1_000_000, 100_000_000)?);
            let mut cache = self
                .multi_pack_index
                .write()
                .map_err(|_| midx_error("MIDX cache lock poisoned"))?;
            Arc::clone(cache.get_or_insert(parsed))
        };
        let Some(entry) = midx.find(id) else {
            return Ok(None);
        };
        let name = midx
            .pack_name(entry.pack_index)
            .ok_or_else(|| midx_error("pack ID out of bounds"))?;
        Ok(Some((
            self.git_path("objects/pack")
                .join(name)
                .with_extension("idx"),
            entry.offset,
        )))
    }
}

#[allow(clippy::too_many_lines)]
fn parse(data: &[u8], max_packs: usize, max_objects: usize) -> Result<MultiPackIndex> {
    if data.len() < 12 + 5 * 12 + 20 || &data[..4] != b"MIDX" {
        return invalid("invalid or truncated header");
    }
    let version = data[4];
    if !matches!(version, 1 | 2) || data[5] != 1 || data[7] != 0 {
        return invalid("unsupported version, hash, or base count");
    }
    let chunks = usize::from(data[6]);
    let packs = usize::try_from(be32(&data[8..12])).expect("u32 fits usize");
    if packs > max_packs {
        return invalid("pack count exceeds limit");
    }
    let table_end = 12usize
        .checked_add(
            (chunks + 1)
                .checked_mul(12)
                .ok_or_else(|| midx_error("chunk table overflow"))?,
        )
        .ok_or_else(|| midx_error("chunk table overflow"))?;
    let payload_end = data
        .len()
        .checked_sub(20)
        .ok_or_else(|| midx_error("truncated checksum"))?;
    if table_end > payload_end || sha1::digest(&data[..payload_end]) != data[payload_end..] {
        return invalid("truncated table or checksum mismatch");
    }
    let mut table = BTreeMap::new();
    let mut previous = table_end;
    for index in 0..=chunks {
        let at = 12 + index * 12;
        let id: [u8; 4] = data[at..at + 4].try_into().expect("bounded");
        let offset = usize::try_from(u64::from_be_bytes(
            data[at + 4..at + 12].try_into().expect("bounded"),
        ))
        .map_err(|_| midx_error("chunk offset overflow"))?;
        if offset < previous || offset > payload_end || offset % 4 != 0 {
            return invalid("invalid chunk offset");
        }
        if index == chunks {
            if id != [0; 4] || offset != payload_end {
                return invalid("invalid chunk terminator");
            }
        } else if id == [0; 4] || table.insert(id, (offset, 0usize)).is_some() {
            return invalid("duplicate or null chunk ID");
        }
        if index > 0 {
            let prior: [u8; 4] = data[12 + (index - 1) * 12..16 + (index - 1) * 12]
                .try_into()
                .expect("bounded");
            if let Some(value) = table.get_mut(&prior) {
                value.1 = offset;
            }
        }
        previous = offset;
    }
    let names_data = chunk(&table, data, *b"PNAM")?;
    let mut pack_names = Vec::with_capacity(packs);
    let mut cursor = 0;
    for _ in 0..packs {
        let end = names_data[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| cursor + position)
            .ok_or_else(|| midx_error("unterminated pack name"))?;
        let name = std::str::from_utf8(&names_data[cursor..end])
            .map_err(|_| midx_error("non-UTF-8 pack name"))?;
        if !is_pack_index_name(name)
            || (version == 1
                && pack_names
                    .last()
                    .is_some_and(|last: &String| last.as_str() >= name))
        {
            return invalid("invalid or unsorted pack names");
        }
        pack_names.push(name.to_owned());
        cursor = end + 1;
    }
    if names_data[cursor..].iter().any(|byte| *byte != 0) {
        return invalid("nonzero pack-name padding");
    }
    let fanout = chunk(&table, data, *b"OIDF")?;
    if fanout.len() != 1024 {
        return invalid("OIDF has wrong size");
    }
    let mut count = 0usize;
    for word in fanout.chunks_exact(4) {
        let value = be32(word) as usize;
        if value < count {
            return invalid("OIDF is not monotonic");
        }
        count = value;
    }
    if count > max_objects {
        return invalid("object count exceeds limit");
    }
    let oids = chunk(&table, data, *b"OIDL")?;
    let offsets = chunk(&table, data, *b"OOFF")?;
    if oids.len() != count * 20 || offsets.len() != count * 8 {
        return invalid("OIDL or OOFF has wrong size");
    }
    let large = table
        .get(b"LOFF")
        .map_or(&[][..], |&(start, end)| &data[start..end]);
    if large.len() % 8 != 0 {
        return invalid("LOFF has wrong size");
    }
    let mut entries = Vec::with_capacity(count);
    let mut prior = None;
    for position in 0..count {
        let id = ObjectId::from_bytes(
            oids[position * 20..position * 20 + 20]
                .try_into()
                .expect("bounded"),
        );
        if prior.is_some_and(|value| value >= id) {
            return invalid("OIDs are not strictly sorted");
        }
        prior = Some(id);
        let pack_index = be32(&offsets[position * 8..position * 8 + 4]);
        if pack_index as usize >= packs {
            return invalid("pack ID out of bounds");
        }
        let raw = be32(&offsets[position * 8 + 4..position * 8 + 8]);
        let offset = if raw & LARGE_OFFSET == 0 {
            u64::from(raw)
        } else {
            let index = (raw & !LARGE_OFFSET) as usize;
            let at = index
                .checked_mul(8)
                .ok_or_else(|| midx_error("large offset overflow"))?;
            let bytes = large
                .get(at..at + 8)
                .ok_or_else(|| midx_error("large offset out of bounds"))?;
            u64::from_be_bytes(bytes.try_into().expect("eight bytes"))
        };
        entries.push(MultiPackIndexEntry {
            id,
            pack_index,
            offset,
        });
    }
    Ok(MultiPackIndex {
        pack_names,
        entries,
    })
}

fn encode(names: &[String], entries: &[MultiPackIndexEntry]) -> Result<Vec<u8>> {
    let mut names_data = Vec::new();
    for name in names {
        names_data.extend_from_slice(name.as_bytes());
        names_data.push(0);
    }
    while names_data.len() % 4 != 0 {
        names_data.push(0);
    }
    let large_values = entries
        .iter()
        .filter(|entry| entry.offset > 0x7fff_ffff)
        .map(|entry| entry.offset)
        .collect::<Vec<_>>();
    let chunks = if large_values.is_empty() { 4 } else { 5 };
    let header = 12 + (chunks + 1) * 12;
    let pnam = header;
    let fanout_offset = pnam + names_data.len();
    let lookup_offset = fanout_offset + 1024;
    let ooff = lookup_offset + entries.len() * 20;
    let loff = ooff + entries.len() * 8;
    let end = loff + large_values.len() * 8;
    let mut out = Vec::with_capacity(end + 20);
    out.extend_from_slice(b"MIDX\x01\x01");
    out.push(u8::try_from(chunks).expect("four or five chunks"));
    out.push(0);
    out.extend_from_slice(
        &u32::try_from(names.len())
            .map_err(|_| midx_error("pack count overflow"))?
            .to_be_bytes(),
    );
    for (id, offset) in [
        (*b"PNAM", pnam),
        (*b"OIDF", fanout_offset),
        (*b"OIDL", lookup_offset),
        (*b"OOFF", ooff),
    ] {
        out.extend_from_slice(&id);
        out.extend_from_slice(&(offset as u64).to_be_bytes());
    }
    if !large_values.is_empty() {
        out.extend_from_slice(b"LOFF");
        out.extend_from_slice(&(loff as u64).to_be_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(end as u64).to_be_bytes());
    out.extend_from_slice(&names_data);
    let mut fanout = [0u32; 256];
    for entry in entries {
        for value in &mut fanout[entry.id.as_bytes()[0] as usize..] {
            *value += 1;
        }
    }
    for value in fanout {
        out.extend_from_slice(&value.to_be_bytes());
    }
    for entry in entries {
        out.extend_from_slice(entry.id.as_bytes());
    }
    let mut large_index = 0u32;
    for entry in entries {
        out.extend_from_slice(&entry.pack_index.to_be_bytes());
        let raw = if entry.offset > 0x7fff_ffff {
            let value = LARGE_OFFSET | large_index;
            large_index += 1;
            value
        } else {
            u32::try_from(entry.offset).map_err(|_| midx_error("offset overflow"))?
        };
        out.extend_from_slice(&raw.to_be_bytes());
    }
    for value in large_values {
        out.extend_from_slice(&value.to_be_bytes());
    }
    let checksum = sha1::digest(&out);
    out.extend_from_slice(&checksum);
    Ok(out)
}

fn is_pack_index_name(name: &str) -> bool {
    name.strip_prefix("pack-")
        .and_then(|value| value.strip_suffix(".idx"))
        .is_some_and(|hash| hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}
fn chunk<'a>(
    table: &BTreeMap<[u8; 4], (usize, usize)>,
    data: &'a [u8],
    id: [u8; 4],
) -> Result<&'a [u8]> {
    let &(start, end) = table
        .get(&id)
        .ok_or_else(|| midx_error("required chunk missing"))?;
    Ok(&data[start..end])
}
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four bytes"))
}
fn midx_error(message: &str) -> Error {
    Error::InvalidRepository(format!("invalid multi-pack-index: {message}"))
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(midx_error(message))
}

#[cfg(test)]
mod tests {
    use super::{MultiPackIndex, MultiPackIndexEntry, encode};
    use crate::{
        InitOptions, MemoryFileSystem, MultiPackIndexOptions, ObjectId, ObjectKind, PackOptions,
        Repository,
    };

    #[test]
    fn round_trips_locations_and_large_offsets() {
        let names = vec![
            "pack-1111111111111111111111111111111111111111.idx".into(),
            "pack-2222222222222222222222222222222222222222.idx".into(),
        ];
        let entries = vec![
            MultiPackIndexEntry {
                id: ObjectId::from_bytes([1; 20]),
                pack_index: 0,
                offset: 12,
            },
            MultiPackIndexEntry {
                id: ObjectId::from_bytes([2; 20]),
                pack_index: 1,
                offset: u64::from(u32::MAX) + 7,
            },
        ];
        let bytes = encode(&names, &entries).unwrap();
        let parsed = MultiPackIndex::parse(&bytes, 2, 2).unwrap();
        assert_eq!(parsed.pack_names(), names);
        assert_eq!(parsed.entries(), entries);
        assert!(MultiPackIndex::parse(&bytes, 1, 2).is_err());
        let mut corrupt = bytes;
        corrupt[20] ^= 1;
        assert!(MultiPackIndex::parse(&corrupt, 2, 2).is_err());
    }

    #[test]
    fn writes_deduplicated_pack_locations_and_accelerates_reads() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let shared = repository
            .write_object(ObjectKind::Blob, b"shared")
            .unwrap();
        let first = repository.write_object(ObjectKind::Blob, b"first").unwrap();
        let second = repository
            .write_object(ObjectKind::Blob, b"second")
            .unwrap();
        let first_pack = repository
            .write_pack(&[shared, first], &PackOptions::default())
            .unwrap();
        let second_pack = repository
            .write_pack(&[shared, second], &PackOptions::default())
            .unwrap();
        let preferred = second_pack
            .index_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let report = repository
            .write_multi_pack_index(&MultiPackIndexOptions {
                preferred_pack: Some(preferred.clone()),
                ..MultiPackIndexOptions::default()
            })
            .unwrap();
        assert_eq!(
            (report.packs, report.objects, report.duplicate_objects),
            (2, 3, 1)
        );
        let midx = repository.read_multi_pack_index(1 << 20, 2, 3).unwrap();
        assert_eq!(
            midx.pack_name(midx.find(shared).unwrap().pack_index()),
            Some(preferred.as_str())
        );
        assert_eq!(
            repository.read_packed_object(second, 4096).unwrap().data(),
            b"second"
        );
        assert_ne!(first_pack.index_path, second_pack.index_path);
        assert!(
            !repository
                .write_multi_pack_index(&MultiPackIndexOptions {
                    preferred_pack: Some(preferred),
                    ..MultiPackIndexOptions::default()
                })
                .unwrap()
                .changed
        );
    }

    #[test]
    fn refuses_to_write_without_nonempty_packs() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        assert!(
            repository
                .write_multi_pack_index(&MultiPackIndexOptions::default())
                .is_err()
        );
    }
}
