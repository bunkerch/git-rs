//! Git cruft-pack per-object modification-time metadata.

use std::collections::BTreeMap;

use crate::object::sha1;
use crate::{Error, ObjectId, PackIndex, Result};

const SIGNATURE: &[u8; 4] = b"MTME";
const VERSION: u32 = 1;
const SHA1_ID: u32 = 1;
const HASH_SIZE: usize = 20;

/// Validated `pack-*.mtimes` data in pack-index object order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CruftMtimes {
    entries: Vec<(ObjectId, u32)>,
    pack_checksum: [u8; HASH_SIZE],
}

impl CruftMtimes {
    /// Parse and verify an mtimes file against its corresponding pack index.
    ///
    /// # Errors
    /// Returns an error for an invalid header, length, checksum, pack checksum,
    /// or object-count mismatch.
    pub fn parse(data: &[u8], index: &PackIndex) -> Result<Self> {
        let count = index.entries().len();
        let expected = 12usize
            .checked_add(count.checked_mul(4).ok_or_else(corrupt)?)
            .and_then(|size| size.checked_add(2 * HASH_SIZE))
            .ok_or_else(corrupt)?;
        if data.len() != expected
            || data.get(..4) != Some(SIGNATURE)
            || read_u32(data, 4)? != VERSION
            || read_u32(data, 8)? != SHA1_ID
        {
            return Err(corrupt());
        }
        let pack_offset = 12 + count * 4;
        if data[pack_offset..pack_offset + HASH_SIZE] != *index.pack_checksum()
            || sha1::digest(&data[..data.len() - HASH_SIZE]) != data[data.len() - HASH_SIZE..]
        {
            return Err(corrupt());
        }
        let entries = index
            .entries()
            .iter()
            .enumerate()
            .map(|(position, entry)| Ok((entry.id, read_u32(data, 12 + position * 4)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            entries,
            pack_checksum: *index.pack_checksum(),
        })
    }

    /// Encode metadata for every index entry. Missing timestamps use zero.
    #[must_use]
    pub fn encode(index: &PackIndex, mtimes: &BTreeMap<ObjectId, u32>) -> Vec<u8> {
        let mut data = Vec::with_capacity(12 + index.entries().len() * 4 + 2 * HASH_SIZE);
        data.extend_from_slice(SIGNATURE);
        data.extend_from_slice(&VERSION.to_be_bytes());
        data.extend_from_slice(&SHA1_ID.to_be_bytes());
        for entry in index.entries() {
            data.extend_from_slice(&mtimes.get(&entry.id).copied().unwrap_or(0).to_be_bytes());
        }
        data.extend_from_slice(index.pack_checksum());
        data.extend_from_slice(&sha1::digest(&data));
        data
    }

    #[must_use]
    pub fn entries(&self) -> &[(ObjectId, u32)] {
        &self.entries
    }
    #[must_use]
    pub const fn pack_checksum(&self) -> &[u8; HASH_SIZE] {
        &self.pack_checksum
    }
    #[must_use]
    pub fn mtime(&self, id: ObjectId) -> Option<u32> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.0)
            .ok()
            .map(|index| self.entries[index].1)
    }
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(corrupt)?;
    Ok(u32::from_be_bytes(
        bytes.try_into().expect("four-byte slice"),
    ))
}

fn corrupt() -> Error {
    Error::InvalidObject("corrupt cruft mtimes file".into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::CruftMtimes;
    use crate::{
        FileSystem, InitOptions, MemoryFileSystem, ObjectKind, PackIndex, PackOptions, Repository,
    };

    #[test]
    fn mtimes_round_trip_in_index_order_and_reject_corruption() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let ids = [
            repository.write_object(ObjectKind::Blob, b"a").unwrap(),
            repository.write_object(ObjectKind::Blob, b"b").unwrap(),
        ];
        let written = repository
            .write_pack(&ids, &PackOptions::default())
            .unwrap();
        let index = PackIndex::parse(&filesystem.read(&written.index_path).unwrap()).unwrap();
        let times = BTreeMap::from([(ids[0], 10), (ids[1], 20)]);
        let encoded = CruftMtimes::encode(&index, &times);
        let parsed = CruftMtimes::parse(&encoded, &index).unwrap();
        assert_eq!(parsed.mtime(ids[0]), Some(10));
        assert_eq!(parsed.mtime(ids[1]), Some(20));
        let mut corrupt = encoded;
        corrupt[12] ^= 1;
        assert!(CruftMtimes::parse(&corrupt, &index).is_err());
    }
}
