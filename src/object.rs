//! Git object identifiers and the loose object database.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::{Error, Repository, Result};

/// A SHA-1 object identifier used by repository format version 0.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId([u8; Self::LENGTH]);

impl ObjectId {
    pub const LENGTH: usize = 20;
    pub const HEX_LENGTH: usize = Self::LENGTH * 2;

    #[must_use]
    pub const fn null() -> Self {
        Self([0; Self::LENGTH])
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    #[must_use]
    pub fn to_hex(self) -> [u8; Self::HEX_LENGTH] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = [0; Self::HEX_LENGTH];
        for (index, byte) in self.0.iter().copied().enumerate() {
            output[index * 2] = HEX[usize::from(byte >> 4)];
            output[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        output
    }

    /// Compute the ID of an object without storing it.
    #[must_use]
    pub fn compute(kind: ObjectKind, data: &[u8]) -> Self {
        Self(sha1::digest(&encode_object(kind, data)))
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.len() != Self::HEX_LENGTH {
            return Err(Error::InvalidObjectId(value.to_owned()));
        }
        let mut bytes = [0; Self::LENGTH];
        for (output, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
            let high =
                hex_value(pair[0]).ok_or_else(|| Error::InvalidObjectId(value.to_owned()))?;
            let low = hex_value(pair[1]).ok_or_else(|| Error::InvalidObjectId(value.to_owned()))?;
            *output = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        let text = std::str::from_utf8(&hex).map_err(|_| fmt::Error)?;
        formatter.write_str(text)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Blob => b"blob",
            Self::Tree => b"tree",
            Self::Commit => b"commit",
            Self::Tag => b"tag",
        }
    }

    fn parse(value: &[u8]) -> Result<Self> {
        match value {
            b"blob" => Ok(Self::Blob),
            b"tree" => Ok(Self::Tree),
            b"commit" => Ok(Self::Commit),
            b"tag" => Ok(Self::Tag),
            _ => Err(Error::InvalidObject(format!(
                "unknown type `{}`",
                String::from_utf8_lossy(value)
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Object {
    kind: ObjectKind,
    data: Vec<u8>,
}

impl Object {
    pub(crate) fn from_parts(kind: ObjectKind, data: Vec<u8>) -> Self {
        Self { kind, data }
    }
    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.kind
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
    /// Write an immutable loose object and return its content-derived ID.
    ///
    /// Existing objects are left untouched. The object is framed, hashed with
    /// SHA-1, zlib-compressed, and stored in Git's fanout object directory.
    ///
    /// # Errors
    /// Returns an error if the object cannot be stored.
    pub fn write_object(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let encoded = encode_object(kind, data);
        let id = ObjectId(sha1::digest(&encoded));
        let path = object_path(id);
        if self.filesystem().exists(&self.git_path(&path))? {
            return Ok(id);
        }
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6);
        match self.write_atomic(&path, &compressed) {
            Ok(()) => Ok(id),
            Err(Error::AlreadyExists(_)) if self.filesystem().exists(&self.git_path(&path))? => {
                Ok(id)
            }
            Err(error) => Err(error),
        }
    }

    /// Read and verify an object, transparently following enabled replace refs.
    ///
    /// # Errors
    /// Returns an error for missing, malformed, oversized, corrupt, or
    /// non-loose objects and for storage failures.
    pub fn read_object(&self, id: ObjectId, max_size: usize) -> Result<Object> {
        let resolved = self.resolve_replacement(id)?;
        self.read_object_raw(resolved, max_size)
    }

    /// Read an object by its actual ID without consulting replace refs.
    ///
    /// This is intended for integrity checking and replace-ref administration.
    /// Most callers should use [`Repository::read_object`].
    ///
    /// # Errors
    /// Returns an error for missing, malformed, oversized, corrupt, or
    /// unsupported object storage, or for filesystem failures.
    pub fn read_object_raw(&self, id: ObjectId, max_size: usize) -> Result<Object> {
        let compressed = match self.filesystem().read(&self.git_path(object_path(id))) {
            Ok(compressed) => compressed,
            Err(Error::NotFound(_)) => return self.read_packed_object(id, max_size),
            Err(error) => return Err(error),
        };
        let framing = 6 + 1 + 20 + 1;
        let encoded = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
            &compressed,
            max_size.saturating_add(framing),
        )
        .map_err(|error| Error::Compression(format!("{error:?}")))?;

        if ObjectId(sha1::digest(&encoded)) != id {
            return Err(Error::InvalidObject(format!("hash mismatch for {id}")));
        }
        decode_object(encoded, max_size)
    }

    /// Test whether a loose object file exists.
    ///
    /// # Errors
    /// Returns a storage error if existence cannot be determined.
    pub fn contains_loose_object(&self, id: ObjectId) -> Result<bool> {
        self.filesystem().exists(&self.git_path(object_path(id)))
    }
}

fn encode_object(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let size = data.len().to_string();
    let mut encoded = Vec::with_capacity(kind.as_bytes().len() + 1 + size.len() + 1 + data.len());
    encoded.extend_from_slice(kind.as_bytes());
    encoded.push(b' ');
    encoded.extend_from_slice(size.as_bytes());
    encoded.push(0);
    encoded.extend_from_slice(data);
    encoded
}

fn decode_object(mut encoded: Vec<u8>, max_size: usize) -> Result<Object> {
    let nul = encoded
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| Error::InvalidObject("missing header terminator".into()))?;
    let header = &encoded[..nul];
    let separator = header
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| Error::InvalidObject("missing type/size separator".into()))?;
    let kind = ObjectKind::parse(&header[..separator])?;
    let size_text = std::str::from_utf8(&header[separator + 1..])
        .map_err(|_| Error::InvalidObject("size is not ASCII".into()))?;
    if size_text.is_empty()
        || (size_text.len() > 1 && size_text.starts_with('0'))
        || !size_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::InvalidObject("non-canonical object size".into()));
    }
    let declared = size_text
        .parse::<u64>()
        .map_err(|_| Error::InvalidObject("object size overflows u64".into()))?;
    if declared > max_size as u64 {
        return Err(Error::ObjectTooLarge {
            declared,
            limit: max_size,
        });
    }
    let actual_size = encoded.len() - nul - 1;
    if u64::try_from(actual_size).ok() != Some(declared) {
        return Err(Error::InvalidObject(format!(
            "declared size {declared} differs from actual size {actual_size}"
        )));
    }
    let data = encoded.split_off(nul + 1);
    Ok(Object { kind, data })
}

fn object_path(id: ObjectId) -> PathBuf {
    let hex = id.to_hex();
    let directory = std::str::from_utf8(&hex[..2]).expect("hex is ASCII");
    let file = std::str::from_utf8(&hex[2..]).expect("hex is ASCII");
    PathBuf::from("objects").join(directory).join(file)
}

pub(crate) mod sha1 {
    pub(crate) fn digest(input: &[u8]) -> [u8; 20] {
        let bit_length = (input.len() as u64).wrapping_mul(8);
        let blocks = (input.len() + 9).div_ceil(64);
        let mut padded = Vec::with_capacity(blocks * 64);
        padded.extend_from_slice(input);
        padded.push(0x80);
        padded.resize(blocks * 64 - 8, 0);
        padded.extend_from_slice(&bit_length.to_be_bytes());

        let mut state = [
            0x6745_2301_u32,
            0xefcd_ab89,
            0x98ba_dcfe,
            0x1032_5476,
            0xc3d2_e1f0,
        ];
        for block in padded.chunks_exact(64) {
            compress(&mut state, block);
        }
        let mut output = [0; 20];
        for (chunk, word) in output.chunks_exact_mut(4).zip(state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    #[allow(clippy::many_single_char_names)]
    fn compress(state: &mut [u32; 5], block: &[u8]) {
        let mut words = [0_u32; 80];
        for (word, bytes) in words.iter_mut().take(16).zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for (index, word) in words.into_iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use super::{ObjectId, ObjectKind, object_path, sha1};
    use crate::{Error, FileSystem, InitOptions, MemoryFileSystem, Repository};

    #[test]
    fn parses_and_formats_git_hex_object_ids() {
        let text = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let id = ObjectId::from_str(text).unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(id.as_bytes()[0..4], [0xe6, 0x9d, 0xe2, 0x9b]);
    }

    #[test]
    fn accepts_uppercase_input_and_canonicalizes_to_lowercase() {
        let id = ObjectId::from_str("E69DE29BB2D1D6434B8B29AE775AD8C2E48C5391").unwrap();
        assert_eq!(id.to_string(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn rejects_wrong_length_and_non_hex_input() {
        assert!(ObjectId::from_str("abc").is_err());
        assert!(ObjectId::from_str("g69de29bb2d1d6434b8b29ae775ad8c2e48c5391").is_err());
    }

    #[test]
    fn matches_fips_sha1_vectors() {
        assert_eq!(
            hex(sha1::digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(sha1::digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(sha1::digest(b"The quick brown fox jumps over the lazy dog")),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn writes_git_compatible_loose_blobs_and_reads_them_back() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let empty = repository.write_object(ObjectKind::Blob, b"").unwrap();
        let hello = repository
            .write_object(ObjectKind::Blob, b"hello\n")
            .unwrap();
        assert_eq!(
            empty.to_string(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            hello.to_string(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(
            repository.read_object(hello, 1024).unwrap().data(),
            b"hello\n"
        );
        let path = PathBuf::from("repo/.git/objects/ce/013625030ba8dba906f756967f9e9ca394464a");
        assert!(fs.exists(&path).unwrap());
    }

    #[test]
    fn rejects_reads_above_the_callers_size_limit() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let id = repository
            .write_object(ObjectKind::Blob, &[7; 128])
            .unwrap();
        assert!(repository.read_object(id, 64).is_err());
    }

    #[test]
    fn detects_corrupt_loose_object_content() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let id = repository
            .write_object(ObjectKind::Blob, b"original")
            .unwrap();
        let corrupt = miniz_oxide::deflate::compress_to_vec_zlib(b"blob 7\0changed", 6);
        fs.write(&repository.git_path(object_path(id)), &corrupt)
            .unwrap();
        assert!(matches!(
            repository.read_object(id, 1024),
            Err(Error::InvalidObject(_))
        ));
    }

    fn hex(bytes: [u8; 20]) -> String {
        ObjectId::from_bytes(bytes).to_string()
    }

    #[test]
    fn object_path_uses_git_fanout() {
        let id = ObjectId::from_str("ce013625030ba8dba906f756967f9e9ca394464a").unwrap();
        assert_eq!(
            object_path(id),
            Path::new("objects/ce/013625030ba8dba906f756967f9e9ca394464a")
        );
    }
}
