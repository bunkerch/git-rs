//! Git directory cache (index) versions 2, 3, and 4.

use std::cmp::Ordering;
use std::path::Path;

use crate::fs::is_ntfs_dotgit;
use crate::object::sha1;
use crate::{Error, ObjectId, Repository, Result};

const SIGNATURE: &[u8; 4] = b"DIRC";
const HEADER_SIZE: usize = 12;
const CHECKSUM_SIZE: usize = ObjectId::LENGTH;
const ENTRY_FIXED_SIZE: usize = 62;
const NAME_MASK: u16 = 0x0fff;
const STAGE_MASK: u16 = 0x3000;
const EXTENDED: u16 = 0x4000;
const ASSUME_VALID: u16 = 0x8000;
const INTENT_TO_ADD: u16 = 0x2000;
const SKIP_WORKTREE: u16 = 0x4000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum IndexVersion {
    V2 = 2,
    V3 = 3,
    V4 = 4,
}

impl TryFrom<u32> for IndexVersion {
    type Error = Error;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            _ => Err(Error::InvalidRepository(format!(
                "unsupported index version {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatData {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub device: u32,
    pub inode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexEntry {
    stat: StatData,
    mode: u32,
    id: ObjectId,
    path: Vec<u8>,
    stage: u8,
    assume_valid: bool,
    intent_to_add: bool,
    skip_worktree: bool,
}

impl IndexEntry {
    /// Create a stage-zero index entry.
    ///
    /// # Errors
    /// Returns an error for an unsafe repository path or unsupported mode.
    pub fn new(path: impl Into<Vec<u8>>, mode: u32, id: ObjectId, stat: StatData) -> Result<Self> {
        Self::with_stage(path, mode, id, stat, 0)
    }

    /// Create an index entry at merge stage 0–3.
    ///
    /// # Errors
    /// Returns an error for an invalid stage, unsafe path, or unsupported mode.
    pub fn with_stage(
        path: impl Into<Vec<u8>>,
        mode: u32,
        id: ObjectId,
        stat: StatData,
        stage: u8,
    ) -> Result<Self> {
        let path = path.into();
        validate_path(&path)?;
        if stage > 3 {
            return Err(Error::InvalidRepository("index stage exceeds 3".into()));
        }
        if !matches!(
            mode,
            0o100_644 | 0o100_755 | 0o120_000 | 0o160_000 | 0o040_000
        ) {
            return Err(Error::InvalidRepository(format!(
                "unsupported index mode {mode:o}"
            )));
        }
        Ok(Self {
            stat,
            mode,
            id,
            path,
            stage,
            assume_valid: false,
            intent_to_add: false,
            skip_worktree: false,
        })
    }

    #[must_use]
    pub const fn stat(&self) -> StatData {
        self.stat
    }
    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.mode
    }
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
    #[must_use]
    pub const fn stage(&self) -> u8 {
        self.stage
    }
    #[must_use]
    pub const fn assume_valid(&self) -> bool {
        self.assume_valid
    }
    #[must_use]
    pub const fn intent_to_add(&self) -> bool {
        self.intent_to_add
    }
    #[must_use]
    pub const fn skip_worktree(&self) -> bool {
        self.skip_worktree
    }

    #[must_use]
    pub const fn with_assume_valid(mut self, value: bool) -> Self {
        self.assume_valid = value;
        self
    }

    #[must_use]
    pub const fn with_intent_to_add(mut self, value: bool) -> Self {
        self.intent_to_add = value;
        self
    }

    #[must_use]
    pub const fn with_skip_worktree(mut self, value: bool) -> Self {
        self.skip_worktree = value;
        self
    }

    #[must_use]
    pub const fn with_stat(mut self, value: StatData) -> Self {
        self.stat = value;
        self
    }

    /// Replace the regular-file executable bit while preserving other data.
    ///
    /// # Errors
    /// Returns an error when this is not a regular-file entry.
    pub fn with_executable(mut self, value: bool) -> Result<Self> {
        if !matches!(self.mode, 0o100_644 | 0o100_755) {
            return Err(Error::InvalidRepository(
                "executable bit requires a regular-file index entry".into(),
            ));
        }
        self.mode = if value { 0o100_755 } else { 0o100_644 };
        Ok(self)
    }

    /// Replace the repository path while preserving object, mode, stat, stage,
    /// and extended flags.
    ///
    /// # Errors
    /// Returns an error when the new path is not index-safe.
    pub fn with_path(mut self, path: impl Into<Vec<u8>>) -> Result<Self> {
        let path = path.into();
        validate_path(&path)?;
        self.path = path;
        Ok(self)
    }

    fn has_extended_flags(&self) -> bool {
        self.intent_to_add || self.skip_worktree
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexExtension {
    signature: [u8; 4],
    data: Vec<u8>,
}

impl IndexExtension {
    /// Create an index extension with its four-byte signature.
    ///
    /// # Errors
    /// Returns an error if the signature is not four ASCII letters.
    pub fn new(signature: [u8; 4], data: Vec<u8>) -> Result<Self> {
        if !signature.iter().all(u8::is_ascii_alphabetic) {
            return Err(Error::InvalidRepository(
                "invalid index extension signature".into(),
            ));
        }
        Ok(Self { signature, data })
    }

    #[must_use]
    pub const fn signature(&self) -> &[u8; 4] {
        &self.signature
    }
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Index {
    version: IndexVersion,
    entries: Vec<IndexEntry>,
    extensions: Vec<IndexExtension>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            version: IndexVersion::V2,
            entries: Vec::new(),
            extensions: Vec::new(),
        }
    }
}

impl Index {
    /// Create a sorted index without extensions.
    ///
    /// # Errors
    /// Returns an error for duplicate path/stage pairs or v2 extended flags.
    pub fn new(version: IndexVersion, mut entries: Vec<IndexEntry>) -> Result<Self> {
        entries.sort_unstable_by(compare_entries);
        if entries
            .windows(2)
            .any(|pair| compare_entries(&pair[0], &pair[1]) == Ordering::Equal)
        {
            return Err(Error::InvalidRepository(
                "duplicate index path and stage".into(),
            ));
        }
        if version == IndexVersion::V2 && entries.iter().any(IndexEntry::has_extended_flags) {
            return Err(Error::InvalidRepository(
                "index v2 cannot store extended entry flags".into(),
            ));
        }
        Ok(Self {
            version,
            entries,
            extensions: Vec::new(),
        })
    }

    #[must_use]
    pub const fn version(&self) -> IndexVersion {
        self.version
    }
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }
    #[must_use]
    pub fn extensions(&self) -> &[IndexExtension] {
        &self.extensions
    }

    /// Replace entries while preserving optional index extensions.
    ///
    /// # Errors
    /// Returns the same validation errors as [`Self::new`].
    pub fn with_entries(self, entries: Vec<IndexEntry>) -> Result<Self> {
        let mut replacement = Self::new(self.version, entries)?;
        replacement.extensions = self.extensions;
        Ok(replacement)
    }

    /// Replace the format version and entries while preserving extensions.
    ///
    /// # Errors
    /// Returns the same validation errors as [`Self::new`].
    pub fn with_version_and_entries(
        self,
        version: IndexVersion,
        entries: Vec<IndexEntry>,
    ) -> Result<Self> {
        let mut replacement = Self::new(version, entries)?;
        replacement.extensions = self.extensions;
        Ok(replacement)
    }

    /// Decode and checksum-verify an index file.
    ///
    /// # Errors
    /// Returns an error for bad checksums, unsupported versions, malformed
    /// entries, unsafe paths, ordering violations, or truncated extensions.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE + CHECKSUM_SIZE || &data[..4] != SIGNATURE {
            return Err(Error::InvalidRepository("invalid index header".into()));
        }
        let payload_end = data.len() - CHECKSUM_SIZE;
        if sha1::digest(&data[..payload_end]) != data[payload_end..] {
            return Err(Error::InvalidRepository("index checksum mismatch".into()));
        }
        let version = IndexVersion::try_from(read_u32(data, 4)?)?;
        let count = usize::try_from(read_u32(data, 8)?)
            .map_err(|_| Error::InvalidRepository("index entry count overflows usize".into()))?;
        let mut cursor = HEADER_SIZE;
        let mut entries = Vec::with_capacity(count);
        let mut previous = Vec::new();
        for _ in 0..count {
            let (entry, consumed) = parse_entry(&data[cursor..payload_end], version, &previous)?;
            cursor = cursor
                .checked_add(consumed)
                .ok_or_else(|| Error::InvalidRepository("index offset overflow".into()))?;
            previous.clone_from(&entry.path);
            entries.push(entry);
        }
        let mut index = Self::new(version, entries.clone())?;
        if index.entries != entries {
            return Err(Error::InvalidRepository(
                "index entries are not sorted".into(),
            ));
        }
        while cursor < payload_end {
            if payload_end - cursor < 8 {
                return Err(Error::InvalidRepository("truncated index extension".into()));
            }
            let signature: [u8; 4] = data[cursor..cursor + 4]
                .try_into()
                .map_err(|_| Error::InvalidRepository("invalid extension signature".into()))?;
            if signature == *b"link" {
                return Err(Error::InvalidRepository(
                    "split-index link extension requires base-index merging".into(),
                ));
            }
            if signature[0].is_ascii_lowercase() && signature != *b"sdir" {
                return Err(Error::InvalidRepository(format!(
                    "unknown required index extension `{}`",
                    String::from_utf8_lossy(&signature)
                )));
            }
            let size = usize::try_from(read_u32(data, cursor + 4)?)
                .map_err(|_| Error::InvalidRepository("extension size overflows usize".into()))?;
            cursor += 8;
            let end = cursor
                .checked_add(size)
                .ok_or_else(|| Error::InvalidRepository("extension offset overflow".into()))?;
            if end > payload_end {
                return Err(Error::InvalidRepository(
                    "truncated index extension data".into(),
                ));
            }
            index
                .extensions
                .push(IndexExtension::new(signature, data[cursor..end].to_vec())?);
            cursor = end;
        }
        Ok(index)
    }

    /// Encode entries, extensions, and the trailing SHA-1 checksum.
    ///
    /// # Errors
    /// Returns an error if counts or extension sizes exceed the v2-v4 format.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.entries.len())
            .map_err(|_| Error::InvalidRepository("too many index entries".into()))?;
        let mut data = Vec::with_capacity(HEADER_SIZE + self.entries.len() * 80 + CHECKSUM_SIZE);
        data.extend_from_slice(SIGNATURE);
        data.extend_from_slice(&(self.version as u32).to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        let mut previous = Vec::new();
        for entry in &self.entries {
            encode_entry(&mut data, entry, self.version, &previous)?;
            previous.clone_from(&entry.path);
        }
        for extension in &self.extensions {
            data.extend_from_slice(&extension.signature);
            let size = u32::try_from(extension.data.len())
                .map_err(|_| Error::InvalidRepository("index extension is too large".into()))?;
            data.extend_from_slice(&size.to_be_bytes());
            data.extend_from_slice(&extension.data);
        }
        data.extend_from_slice(&sha1::digest(&data));
        Ok(data)
    }
}

impl Repository {
    /// Read `.git/index`; an absent file yields an empty v2 index.
    ///
    /// # Errors
    /// Returns an error for malformed index data or storage failures.
    pub fn read_index(&self) -> Result<Index> {
        match self.filesystem().read(&self.git_path("index")) {
            Ok(data) => Index::parse(&data),
            Err(Error::NotFound(_)) => Ok(Index::default()),
            Err(error) => Err(error),
        }
    }

    /// Atomically replace `.git/index` with a checksum-protected index.
    ///
    /// # Errors
    /// Returns an error when encoding or storage publication fails.
    pub fn write_index(&self, index: &Index) -> Result<()> {
        self.write_atomic(Path::new("index"), &index.encode()?)
    }
}

fn parse_entry(data: &[u8], version: IndexVersion, previous: &[u8]) -> Result<(IndexEntry, usize)> {
    if data.len() < ENTRY_FIXED_SIZE {
        return Err(Error::InvalidRepository("truncated index entry".into()));
    }
    let stat = StatData {
        ctime_seconds: read_u32(data, 0)?,
        ctime_nanoseconds: read_u32(data, 4)?,
        mtime_seconds: read_u32(data, 8)?,
        mtime_nanoseconds: read_u32(data, 12)?,
        device: read_u32(data, 16)?,
        inode: read_u32(data, 20)?,
        uid: read_u32(data, 28)?,
        gid: read_u32(data, 32)?,
        size: read_u32(data, 36)?,
    };
    let mode = read_u32(data, 24)?;
    let mut oid = [0; ObjectId::LENGTH];
    oid.copy_from_slice(&data[40..60]);
    let flags = read_u16(data, 60)?;
    let mut cursor = ENTRY_FIXED_SIZE;
    let (intent_to_add, skip_worktree) = if flags & EXTENDED != 0 {
        if version == IndexVersion::V2 || data.len() < cursor + 2 {
            return Err(Error::InvalidRepository(
                "invalid extended index flags".into(),
            ));
        }
        let extended = read_u16(data, cursor)?;
        cursor += 2;
        if extended & !(INTENT_TO_ADD | SKIP_WORKTREE) != 0 {
            return Err(Error::InvalidRepository(
                "unknown extended index flags".into(),
            ));
        }
        (extended & INTENT_TO_ADD != 0, extended & SKIP_WORKTREE != 0)
    } else {
        (false, false)
    };

    let path = if version == IndexVersion::V4 {
        let (strip, used) = decode_varint(&data[cursor..])?;
        cursor += used;
        let strip = usize::try_from(strip)
            .map_err(|_| Error::InvalidRepository("v4 strip length overflows usize".into()))?;
        if strip > previous.len() {
            return Err(Error::InvalidRepository(
                "v4 path strips beyond previous path".into(),
            ));
        }
        let nul = data[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| Error::InvalidRepository("unterminated v4 index path".into()))?;
        let mut path = previous[..previous.len() - strip].to_vec();
        path.extend_from_slice(&data[cursor..cursor + nul]);
        cursor += nul + 1;
        path
    } else {
        let nul = data[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| Error::InvalidRepository("unterminated index path".into()))?;
        let path = data[cursor..cursor + nul].to_vec();
        cursor += nul + 1;
        let padded = cursor.div_ceil(8) * 8;
        if padded > data.len() || data[cursor..padded].iter().any(|byte| *byte != 0) {
            return Err(Error::InvalidRepository(
                "invalid index entry padding".into(),
            ));
        }
        cursor = padded;
        path
    };
    let mut entry = IndexEntry::with_stage(
        path,
        mode,
        ObjectId::from_bytes(oid),
        stat,
        ((flags & STAGE_MASK) >> 12) as u8,
    )?;
    entry.assume_valid = flags & ASSUME_VALID != 0;
    entry.intent_to_add = intent_to_add;
    entry.skip_worktree = skip_worktree;
    let advertised = usize::from(flags & NAME_MASK);
    if advertised != usize::from(NAME_MASK) && advertised != entry.path.len() {
        return Err(Error::InvalidRepository(
            "index path length flag mismatch".into(),
        ));
    }
    Ok((entry, cursor))
}

fn encode_entry(
    data: &mut Vec<u8>,
    entry: &IndexEntry,
    version: IndexVersion,
    previous: &[u8],
) -> Result<()> {
    let start = data.len();
    for value in [
        entry.stat.ctime_seconds,
        entry.stat.ctime_nanoseconds,
        entry.stat.mtime_seconds,
        entry.stat.mtime_nanoseconds,
        entry.stat.device,
        entry.stat.inode,
        entry.mode,
        entry.stat.uid,
        entry.stat.gid,
        entry.stat.size,
    ] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    data.extend_from_slice(entry.id.as_bytes());
    let extended = entry.has_extended_flags();
    if version == IndexVersion::V2 && extended {
        return Err(Error::InvalidRepository(
            "index v2 cannot store extended flags".into(),
        ));
    }
    let name_length = u16::try_from(entry.path.len().min(usize::from(NAME_MASK)))
        .expect("name length is capped at u16 value");
    let mut flags = name_length | (u16::from(entry.stage) << 12);
    if extended {
        flags |= EXTENDED;
    }
    if entry.assume_valid {
        flags |= ASSUME_VALID;
    }
    data.extend_from_slice(&flags.to_be_bytes());
    if extended {
        let mut flags2 = 0;
        if entry.intent_to_add {
            flags2 |= INTENT_TO_ADD;
        }
        if entry.skip_worktree {
            flags2 |= SKIP_WORKTREE;
        }
        data.extend_from_slice(&flags2.to_be_bytes());
    }
    if version == IndexVersion::V4 {
        let common = entry
            .path
            .iter()
            .zip(previous)
            .take_while(|(left, right)| left == right)
            .count();
        encode_varint(
            u64::try_from(previous.len() - common)
                .map_err(|_| Error::InvalidRepository("v4 path is too long".into()))?,
            data,
        );
        data.extend_from_slice(&entry.path[common..]);
        data.push(0);
    } else {
        data.extend_from_slice(&entry.path);
        data.push(0);
        data.resize(start + (data.len() - start).div_ceil(8) * 8, 0);
    }
    Ok(())
}

pub(crate) fn validate_path(path: &[u8]) -> Result<()> {
    if path.is_empty()
        || path[0] == b'/'
        || path.contains(&0)
        || path.contains(&b'\\')
        || path
            .split(|byte| *byte == b'/' || *byte == b'\\')
            .any(|part| {
                part.is_empty() || matches!(part, b"." | b"..") || is_ntfs_dotgit(part)
            })
    {
        return Err(Error::InvalidRepository("unsafe index path".into()));
    }
    Ok(())
}

fn compare_entries(left: &IndexEntry, right: &IndexEntry) -> Ordering {
    left.path
        .cmp(&right.path)
        .then(left.stage.cmp(&right.stage))
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    data.get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or_else(|| Error::InvalidRepository("truncated index integer".into()))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| Error::InvalidRepository("truncated index integer".into()))
}

fn decode_varint(data: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in data.iter().copied().enumerate().take(10) {
        if index == 0 {
            value = u64::from(byte & 0x7f);
        } else {
            value = value
                .checked_add(1)
                .and_then(|value| value.checked_shl(7))
                .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
                .ok_or_else(|| Error::InvalidRepository("v4 varint overflow".into()))?;
        }
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(Error::InvalidRepository("unterminated v4 varint".into()))
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    let mut bytes = [0_u8; 10];
    let mut position = bytes.len() - 1;
    bytes[position] = (value & 0x7f) as u8;
    while {
        value >>= 7;
        value != 0
    } {
        value -= 1;
        position -= 1;
        bytes[position] = 0x80 | (value & 0x7f) as u8;
    }
    output.extend_from_slice(&bytes[position..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem};
    use std::str::FromStr;

    fn entry(path: &[u8], stage: u8) -> IndexEntry {
        IndexEntry::with_stage(
            path.to_vec(),
            0o100_644,
            ObjectId::from_str("1111111111111111111111111111111111111111").unwrap(),
            StatData {
                size: 42,
                ..StatData::default()
            },
            stage,
        )
        .unwrap()
    }

    #[test]
    fn round_trips_versions_two_three_and_four() {
        for version in [IndexVersion::V2, IndexVersion::V3, IndexVersion::V4] {
            let index = Index::new(version, vec![entry(b"a", 0), entry(b"dir/file", 0)]).unwrap();
            assert_eq!(Index::parse(&index.encode().unwrap()).unwrap(), index);
        }
    }

    #[test]
    fn round_trips_conflict_stages_long_paths_and_extended_flags() {
        let long = vec![b'x'; 5000];
        let entries = vec![
            entry(b"conflict", 1),
            entry(b"conflict", 2),
            entry(b"conflict", 3),
            entry(&long, 0)
                .with_assume_valid(true)
                .with_intent_to_add(true)
                .with_skip_worktree(true),
        ];
        let index = Index::new(IndexVersion::V4, entries).unwrap();
        assert_eq!(Index::parse(&index.encode().unwrap()).unwrap(), index);
    }

    #[test]
    fn preserves_extensions_and_rejects_checksum_corruption() {
        let mut index = Index::new(IndexVersion::V2, vec![entry(b"file", 0)]).unwrap();
        index
            .extensions
            .push(IndexExtension::new(*b"TREE", b"cache".to_vec()).unwrap());
        let encoded = index.encode().unwrap();
        assert_eq!(Index::parse(&encoded).unwrap(), index);
        let mut corrupt = encoded;
        corrupt[20] ^= 1;
        assert!(Index::parse(&corrupt).is_err());
    }

    #[test]
    fn repository_uses_atomic_index_storage() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let index = Index::new(IndexVersion::V2, vec![entry(b"file", 0)]).unwrap();
        repository.write_index(&index).unwrap();
        assert_eq!(repository.read_index().unwrap(), index);
        assert!(!fs.exists(Path::new("repo/.git/index.lock")).unwrap());
    }

    #[test]
    fn git_varint_vectors_round_trip() {
        for value in [0, 1, 127, 128, 255, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut encoded = Vec::new();
            encode_varint(value, &mut encoded);
            assert_eq!(decode_varint(&encoded).unwrap(), (value, encoded.len()));
        }
    }

    #[test]
    fn rejects_win32_normalized_git_aliases() {
        let oid = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        let stat = StatData {
            size: 42,
            ..StatData::default()
        };
        for path in [
            b".git.".as_slice(),
            b".git ".as_slice(),
            b".Git.".as_slice(),
            b"git~1".as_slice(),
            b".git::$INDEX_ALLOCATION".as_slice(),
            b".git:$DATA".as_slice(),
            b".git:stream".as_slice(),
            b"dir/.git.".as_slice(),
            b"dir\\.git".as_slice(),
        ] {
            assert!(
                IndexEntry::with_stage(path.to_vec(), 0o100_644, oid, stat, 0).is_err(),
                "expected {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_unrelated_dotfiles_and_incomplete_short_names() {
        let oid = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        let stat = StatData {
            size: 42,
            ..StatData::default()
        };
        for path in [
            b".gitignore".as_slice(),
            b".gitmodules".as_slice(),
            b".gitattributes".as_slice(),
            b"foo.git".as_slice(),
            b"..git".as_slice(),
            b"git~1x".as_slice(),
            b"git~2".as_slice(),
            b"git~12".as_slice(),
            b"subdir/.git_config".as_slice(),
        ] {
            assert!(
                IndexEntry::with_stage(path.to_vec(), 0o100_644, oid, stat, 0).is_ok(),
                "expected {path:?} to be accepted"
            );
        }
    }

    #[test]
    fn parse_rejects_crafted_v2_index_with_win32_git_aliases() {
        for path in [
            b".git./hooks/pre-commit".as_slice(),
            b".git /hooks/pre-commit".as_slice(),
            b"git~1/hooks/pre-commit".as_slice(),
            b".git::$INDEX_ALLOCATION".as_slice(),
        ] {
            assert!(
                Index::parse(&crafted_v2_index(path)).is_err(),
                "expected crafted index with {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn parse_accepts_crafted_v2_index_with_benign_dotfiles() {
        let index = Index::parse(&crafted_v2_index(b".gitignore")).unwrap();
        assert_eq!(index.entries[0].path, b".gitignore");
    }

    fn crafted_v2_index(path: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"DIRC");
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        let mut entry = Vec::new();
        for value in [0u32, 0, 0, 0, 0, 0, 0o100_644, 0, 0, 19] {
            entry.extend_from_slice(&value.to_be_bytes());
        }
        entry.extend_from_slice(&[0x11; 20]);
        entry.extend_from_slice(
            &u16::try_from(path.len()).expect("test path fits in u16").to_be_bytes(),
        );
        entry.extend_from_slice(path);
        entry.push(0);
        entry.resize(entry.len().div_ceil(8) * 8, 0);
        body.extend_from_slice(&entry);
        let mut data = body.clone();
        data.extend_from_slice(&sha1::digest(&body));
        data
    }
}
