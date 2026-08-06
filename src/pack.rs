//! Validated Git pack index and packed-object reading.

use std::path::{Path, PathBuf};

use crate::object::sha1;
use crate::{Error, Object, ObjectId, ObjectKind, Repository, Result};

const INDEX_MAGIC: [u8; 4] = [0xff, b't', b'O', b'c'];
const PACK_HEADER_SIZE: usize = 12;
const HASH_SIZE: usize = 20;
const MAX_DELTA_DEPTH: usize = 64;

/// Controls pack construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackOptions {
    /// Maximum size accepted while loading each source object.
    pub max_object_size: usize,
    /// Try compact, depth-one deltas against an earlier object of the same type.
    pub use_deltas: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024 * 1024,
            use_deltas: true,
        }
    }
}

/// Complete Git-compatible pack and version-2 index bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackBundle {
    pack: Vec<u8>,
    index: Vec<u8>,
    checksum: [u8; HASH_SIZE],
    object_count: usize,
}

impl PackBundle {
    #[must_use]
    pub fn pack(&self) -> &[u8] {
        &self.pack
    }

    #[must_use]
    pub fn index(&self) -> &[u8] {
        &self.index
    }

    #[must_use]
    pub const fn checksum(&self) -> &[u8; HASH_SIZE] {
        &self.checksum
    }

    #[must_use]
    pub const fn object_count(&self) -> usize {
        self.object_count
    }

    #[must_use]
    pub fn stem(&self) -> String {
        format!("pack-{}", hex_hash(self.checksum))
    }
}

/// Paths of a pack published in a repository object database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrittenPack {
    pub pack_path: PathBuf,
    pub index_path: PathBuf,
    pub checksum: [u8; HASH_SIZE],
    pub object_count: usize,
}

/// An object location and integrity checksum from a pack index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackIndexEntry {
    pub id: ObjectId,
    pub crc32: u32,
    pub offset: u64,
}

/// A fully validated version-2 Git pack index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackIndex {
    entries: Vec<PackIndexEntry>,
    pack_checksum: [u8; HASH_SIZE],
}

impl PackIndex {
    /// Parse an index, validating its checksum, fanout, ordering, and offsets.
    ///
    /// # Errors
    /// Returns an error when any index invariant is violated.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 + 256 * 4 + 2 * HASH_SIZE || data[..4] != INDEX_MAGIC {
            return invalid("missing version-2 index header");
        }
        if be_u32(data, 4)? != 2 {
            return invalid("unsupported pack index version");
        }
        if sha1::digest(&data[..data.len() - HASH_SIZE]) != data[data.len() - HASH_SIZE..] {
            return invalid("pack index checksum mismatch");
        }

        let mut previous = 0_u32;
        for bucket in 0..256 {
            let count = be_u32(data, 8 + bucket * 4)?;
            if count < previous {
                return invalid("non-monotonic pack index fanout");
            }
            previous = count;
        }
        let count = usize::try_from(previous).map_err(|_| pack_error("object count overflow"))?;
        let ids_start = 8 + 256 * 4;
        let crcs_start = checked_add(ids_start, checked_mul(count, HASH_SIZE)?)?;
        let offsets_start = checked_add(crcs_start, checked_mul(count, 4)?)?;
        let large_start = checked_add(offsets_start, checked_mul(count, 4)?)?;
        if large_start > data.len().saturating_sub(2 * HASH_SIZE) {
            return invalid("truncated pack index tables");
        }
        let trailer_start = data.len() - 2 * HASH_SIZE;
        let large_bytes = trailer_start - large_start;
        if !large_bytes.is_multiple_of(8) {
            return invalid("misaligned large-offset table");
        }
        let large_count = large_bytes / 8;

        let mut entries = Vec::with_capacity(count);
        let mut last_id = None;
        let mut fanout = [0_u32; 256];
        for index in 0..count {
            let id_pos = ids_start + index * HASH_SIZE;
            let id = ObjectId::from_bytes(read_hash(data, id_pos)?);
            if last_id.is_some_and(|last| last >= id) {
                return invalid("object IDs are not strictly sorted");
            }
            last_id = Some(id);
            fanout[usize::from(id.as_bytes()[0])] += 1;
            let raw_offset = be_u32(data, offsets_start + index * 4)?;
            let offset = if raw_offset & 0x8000_0000 == 0 {
                u64::from(raw_offset)
            } else {
                let large_index = (raw_offset & 0x7fff_ffff) as usize;
                if large_index >= large_count {
                    return invalid("large-offset index is out of range");
                }
                be_u64(data, large_start + large_index * 8)?
            };
            entries.push(PackIndexEntry {
                id,
                crc32: be_u32(data, crcs_start + index * 4)?,
                offset,
            });
        }
        let mut cumulative = 0_u32;
        for (bucket, count) in fanout.into_iter().enumerate() {
            cumulative += count;
            if be_u32(data, 8 + bucket * 4)? != cumulative {
                return invalid("fanout does not match object IDs");
            }
        }
        Ok(Self {
            entries,
            pack_checksum: read_hash(data, trailer_start)?,
        })
    }

    #[must_use]
    pub fn entries(&self) -> &[PackIndexEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn pack_checksum(&self) -> &[u8; HASH_SIZE] {
        &self.pack_checksum
    }

    #[must_use]
    pub fn find(&self, id: ObjectId) -> Option<PackIndexEntry> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
            .map(|index| self.entries[index])
    }
}

impl Repository {
    /// Build a deterministic pack containing the requested objects.
    ///
    /// Duplicate IDs are emitted once. Source objects may be loose or packed,
    /// and all access remains routed through this repository's filesystem.
    ///
    /// # Errors
    /// Returns an error for an absent or corrupt source object, an excessive
    /// object count, or a source object above the configured size limit.
    pub fn build_pack(&self, ids: &[ObjectId], options: &PackOptions) -> Result<PackBundle> {
        let mut seen = std::collections::BTreeSet::new();
        let mut objects = Vec::with_capacity(ids.len());
        for id in ids.iter().copied() {
            if seen.insert(id) {
                let object = self.read_object(id, options.max_object_size)?;
                objects.push(PackSource {
                    id,
                    kind: object.kind(),
                    data: object.into_data(),
                });
            }
        }
        build_pack(&objects, options)
    }

    /// Build and publish a pack under `objects/pack`.
    ///
    /// The pack is published before its index, so concurrent readers never
    /// discover an index naming an incomplete pack. Content-addressed existing
    /// files are accepted only when their bytes match exactly.
    ///
    /// # Errors
    /// Returns an error when building, validating, or publishing either file
    /// fails.
    pub fn write_pack(&self, ids: &[ObjectId], options: &PackOptions) -> Result<WrittenPack> {
        let bundle = self.build_pack(ids, options)?;
        let stem = bundle.stem();
        let pack_relative = Path::new("objects/pack").join(format!("{stem}.pack"));
        let index_relative = Path::new("objects/pack").join(format!("{stem}.idx"));
        self.publish_pack_file(&pack_relative, bundle.pack())?;
        self.publish_pack_file(&index_relative, bundle.index())?;
        Ok(WrittenPack {
            pack_path: self.git_path(&pack_relative),
            index_path: self.git_path(&index_relative),
            checksum: bundle.checksum,
            object_count: bundle.object_count,
        })
    }

    fn publish_pack_file(&self, relative: &Path, expected: &[u8]) -> Result<()> {
        let path = self.git_path(relative);
        if self.filesystem().exists(&path)? {
            if self.filesystem().read(&path)? == expected {
                return Ok(());
            }
            return invalid("content-addressed pack path contains different bytes");
        }
        let extension = relative
            .extension()
            .and_then(|value| value.to_str())
            .ok_or_else(|| pack_error("pack path has no extension"))?;
        let lock_relative = relative.with_extension(format!("{extension}.lock"));
        let lock_path = self.git_path(&lock_relative);
        self.filesystem().write_new(&lock_path, expected)?;
        if self.filesystem().exists(&path)? {
            let matches = self.filesystem().read(&path)? == expected;
            self.filesystem().remove_file(&lock_path)?;
            return if matches {
                Ok(())
            } else {
                invalid("content-addressed pack path changed during publication")
            };
        }
        if let Err(error) = self.filesystem().rename(&lock_path, &path) {
            let _ = self.filesystem().remove_file(&lock_path);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn read_packed_object(&self, id: ObjectId, max_size: usize) -> Result<Object> {
        let directory = self.git_path("objects/pack");
        let paths = match self.filesystem().read_dir(&directory) {
            Ok(paths) => paths,
            Err(Error::NotFound(_)) => {
                return Err(Error::NotFound(self.git_path(object_label(id))));
            }
            Err(error) => return Err(error),
        };
        for child in paths {
            if child.extension().and_then(|value| value.to_str()) != Some("idx") {
                continue;
            }
            let index_path = directory.join(child);
            let index = PackIndex::parse(&self.filesystem().read(&index_path)?)?;
            if index.find(id).is_none() {
                continue;
            }
            let pack_path = index_path.with_extension("pack");
            let pack = self.filesystem().read(&pack_path)?;
            validate_pack(&pack, &index)?;
            let (kind, data) = resolve(&pack, &index, id, max_size, 0)?;
            if ObjectId::compute(kind, &data) != id {
                return invalid("resolved packed object hash mismatch");
            }
            return Ok(Object::from_parts(kind, data));
        }
        Err(Error::NotFound(self.git_path(object_label(id))))
    }
}

struct PackSource {
    id: ObjectId,
    kind: ObjectKind,
    data: Vec<u8>,
}

fn build_pack(objects: &[PackSource], options: &PackOptions) -> Result<PackBundle> {
    let object_count = u32::try_from(objects.len())
        .map_err(|_| pack_error("pack contains more than u32::MAX objects"))?;
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2_u32.to_be_bytes());
    pack.extend_from_slice(&object_count.to_be_bytes());
    let mut entries: Vec<PackIndexEntry> = Vec::with_capacity(objects.len());
    let mut direct_bases: [Option<usize>; 4] = [None; 4];

    for (position, object) in objects.iter().enumerate() {
        let offset = u64::try_from(pack.len()).map_err(|_| pack_error("pack offset overflow"))?;
        let direct = direct_entry(object.kind, &object.data);
        let kind_slot = kind_slot(object.kind);
        let selected = if options.use_deltas {
            direct_bases[kind_slot]
                .and_then(|base_position| {
                    let base: &PackSource = &objects[base_position];
                    let delta = create_delta(&base.data, &object.data)?;
                    let distance = offset.checked_sub(entries[base_position].offset)?;
                    let candidate = ofs_delta_entry(distance, &delta);
                    (candidate.len() < direct.len()).then_some(candidate)
                })
                .unwrap_or_else(|| direct.clone())
        } else {
            direct.clone()
        };
        let is_direct = selected.len() == direct.len() && selected == direct;
        entries.push(PackIndexEntry {
            id: object.id,
            crc32: crc32(&selected),
            offset,
        });
        pack.extend_from_slice(&selected);
        if is_direct {
            direct_bases[kind_slot] = Some(position);
        }
    }

    let checksum = sha1::digest(&pack);
    pack.extend_from_slice(&checksum);
    let index = encode_index(&entries, checksum)?;
    let parsed = PackIndex::parse(&index)?;
    validate_pack(&pack, &parsed)?;
    Ok(PackBundle {
        pack,
        index,
        checksum,
        object_count: objects.len(),
    })
}

fn direct_entry(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let mut entry = encode_object_header(kind_type(kind), data.len() as u64);
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(data, 6));
    entry
}

fn ofs_delta_entry(distance: u64, delta: &[u8]) -> Vec<u8> {
    let mut entry = encode_object_header(6, delta.len() as u64);
    entry.extend(encode_ofs_distance(distance));
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(delta, 6));
    entry
}

fn encode_object_header(object_type: u8, mut size: u64) -> Vec<u8> {
    let mut byte = (object_type << 4) | u8::try_from(size & 15).expect("four bits fit");
    size >>= 4;
    let mut output = Vec::with_capacity(10);
    while size != 0 {
        output.push(byte | 0x80);
        byte = u8::try_from(size & 0x7f).expect("seven bits fit");
        size >>= 7;
    }
    output.push(byte);
    output
}

fn encode_ofs_distance(mut distance: u64) -> Vec<u8> {
    debug_assert!(distance > 0);
    let mut reversed = vec![(distance & 0x7f) as u8];
    while {
        distance >>= 7;
        distance != 0
    } {
        distance -= 1;
        reversed.push(0x80 | (distance & 0x7f) as u8);
    }
    reversed.reverse();
    reversed
}

fn create_delta(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if base.len() > u32::MAX as usize || target.len() > u32::MAX as usize {
        return None;
    }
    let prefix = base
        .iter()
        .zip(target)
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = base.len().min(target.len()).saturating_sub(prefix);
    let suffix = base[base.len() - max_suffix..]
        .iter()
        .rev()
        .zip(target[target.len() - max_suffix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let mut delta = Vec::new();
    encode_delta_varint(base.len() as u64, &mut delta);
    encode_delta_varint(target.len() as u64, &mut delta);
    emit_copy(0, prefix, &mut delta);
    emit_literals(&target[prefix..target.len() - suffix], &mut delta);
    emit_copy(base.len() - suffix, suffix, &mut delta);
    Some(delta)
}

fn emit_literals(mut data: &[u8], output: &mut Vec<u8>) {
    while !data.is_empty() {
        let length = data.len().min(127);
        output.push(u8::try_from(length).expect("literal chunks are at most 127 bytes"));
        output.extend_from_slice(&data[..length]);
        data = &data[length..];
    }
}

fn emit_copy(mut offset: usize, mut size: usize, output: &mut Vec<u8>) {
    while size != 0 {
        let chunk = size.min(0x00ff_ffff);
        let mut opcode = 0x80_u8;
        let mut parameters = Vec::with_capacity(7);
        for (bit, shift) in [(1, 0), (2, 8), (4, 16), (8, 24)] {
            let byte = u8::try_from((offset >> shift) & 0xff).expect("masked to one byte");
            if byte != 0 {
                opcode |= bit;
                parameters.push(byte);
            }
        }
        if chunk != 0x10000 {
            for (bit, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                let byte = u8::try_from((chunk >> shift) & 0xff).expect("masked to one byte");
                if byte != 0 {
                    opcode |= bit;
                    parameters.push(byte);
                }
            }
        }
        output.push(opcode);
        output.extend(parameters);
        offset += chunk;
        size -= chunk;
    }
}

fn encode_delta_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn encode_index(entries: &[PackIndexEntry], pack_checksum: [u8; HASH_SIZE]) -> Result<Vec<u8>> {
    let mut sorted = entries.to_vec();
    sorted.sort_unstable_by_key(|entry| entry.id);
    if sorted.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return invalid("duplicate object ID while writing index");
    }
    let mut index = Vec::new();
    index.extend_from_slice(&INDEX_MAGIC);
    index.extend_from_slice(&2_u32.to_be_bytes());
    let mut position = 0;
    for bucket in 0..256 {
        while position < sorted.len() && usize::from(sorted[position].id.as_bytes()[0]) <= bucket {
            position += 1;
        }
        index.extend_from_slice(
            &u32::try_from(position)
                .map_err(|_| pack_error("index object count overflow"))?
                .to_be_bytes(),
        );
    }
    for entry in &sorted {
        index.extend_from_slice(entry.id.as_bytes());
    }
    for entry in &sorted {
        index.extend_from_slice(&entry.crc32.to_be_bytes());
    }
    let mut large = Vec::new();
    for entry in &sorted {
        if let Ok(offset) = u32::try_from(entry.offset)
            && offset < 0x8000_0000
        {
            index.extend_from_slice(&offset.to_be_bytes());
        } else {
            let position = u32::try_from(large.len())
                .map_err(|_| pack_error("large-offset table overflow"))?;
            index.extend_from_slice(&(0x8000_0000 | position).to_be_bytes());
            large.push(entry.offset);
        }
    }
    for offset in large {
        index.extend_from_slice(&offset.to_be_bytes());
    }
    index.extend_from_slice(&pack_checksum);
    index.extend_from_slice(&sha1::digest(&index));
    Ok(index)
}

const fn kind_type(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

fn kind_slot(kind: ObjectKind) -> usize {
    usize::from(kind_type(kind) - 1)
}

fn hex_hash(hash: [u8; HASH_SIZE]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(HASH_SIZE * 2);
    for byte in hash {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 15)]));
    }
    text
}

fn validate_pack(pack: &[u8], index: &PackIndex) -> Result<()> {
    if pack.len() < PACK_HEADER_SIZE + HASH_SIZE || &pack[..4] != b"PACK" {
        return invalid("invalid pack header");
    }
    let version = be_u32(pack, 4)?;
    if !(2..=3).contains(&version) {
        return invalid("unsupported pack version");
    }
    if usize::try_from(be_u32(pack, 8)?).ok() != Some(index.entries.len()) {
        return invalid("pack and index object counts differ");
    }
    let trailer = &pack[pack.len() - HASH_SIZE..];
    if sha1::digest(&pack[..pack.len() - HASH_SIZE]) != trailer || trailer != index.pack_checksum {
        return invalid("pack checksum mismatch");
    }
    Ok(())
}

fn resolve(
    pack: &[u8],
    index: &PackIndex,
    id: ObjectId,
    max_size: usize,
    depth: usize,
) -> Result<(ObjectKind, Vec<u8>)> {
    if depth >= MAX_DELTA_DEPTH {
        return invalid("delta chain is too deep");
    }
    let entry = index
        .find(id)
        .ok_or_else(|| pack_error("object absent from index"))?;
    let start = usize::try_from(entry.offset).map_err(|_| pack_error("offset overflow"))?;
    let end = index
        .entries
        .iter()
        .filter_map(|candidate| usize::try_from(candidate.offset).ok())
        .filter(|offset| *offset > start)
        .min()
        .unwrap_or(pack.len() - HASH_SIZE);
    if start < PACK_HEADER_SIZE || start >= end || end > pack.len() - HASH_SIZE {
        return invalid("object offset is outside pack");
    }
    let encoded = &pack[start..end];
    if crc32(encoded) != entry.crc32 {
        return invalid("packed object CRC mismatch");
    }
    let (object_type, declared, mut cursor) = parse_object_header(encoded)?;
    match object_type {
        1..=4 => {
            if declared > max_size as u64 {
                return Err(Error::ObjectTooLarge {
                    declared,
                    limit: max_size,
                });
            }
            let data = inflate(&encoded[cursor..], max_size)?;
            if data.len() as u64 != declared {
                return invalid("packed object size mismatch");
            }
            Ok((kind_from_type(object_type)?, data))
        }
        6 => {
            let distance = parse_ofs_distance(encoded, &mut cursor)?;
            let base_offset = u64::try_from(start)
                .ok()
                .and_then(|offset| offset.checked_sub(distance))
                .ok_or_else(|| pack_error("invalid OFS_DELTA base"))?;
            let base = index
                .entries
                .iter()
                .find(|candidate| candidate.offset == base_offset)
                .ok_or_else(|| pack_error("OFS_DELTA base is absent"))?;
            resolve_delta(
                pack, index, base.id, encoded, cursor, declared, max_size, depth,
            )
        }
        7 => {
            let end = checked_add(cursor, HASH_SIZE)?;
            let bytes = encoded
                .get(cursor..end)
                .ok_or_else(|| pack_error("truncated REF_DELTA"))?;
            let base = ObjectId::from_bytes(
                bytes
                    .try_into()
                    .map_err(|_| pack_error("invalid REF_DELTA base length"))?,
            );
            resolve_delta(pack, index, base, encoded, end, declared, max_size, depth)
        }
        _ => invalid("reserved packed object type"),
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_delta(
    pack: &[u8],
    index: &PackIndex,
    base_id: ObjectId,
    encoded: &[u8],
    cursor: usize,
    declared: u64,
    max_size: usize,
    depth: usize,
) -> Result<(ObjectKind, Vec<u8>)> {
    let (kind, base) = resolve(pack, index, base_id, max_size, depth + 1)?;
    let delta = inflate(&encoded[cursor..], max_size.saturating_mul(2).max(32))?;
    if delta.len() as u64 != declared {
        return invalid("delta instruction size differs from packed header");
    }
    let data = apply_delta(&base, &delta, max_size)?;
    Ok((kind, data))
}

fn parse_object_header(data: &[u8]) -> Result<(u8, u64, usize)> {
    let mut byte = *data
        .first()
        .ok_or_else(|| pack_error("missing object header"))?;
    let object_type = (byte >> 4) & 7;
    let mut size = u64::from(byte & 15);
    let mut shift = 4_u32;
    let mut cursor = 1;
    while byte & 0x80 != 0 {
        byte = *data
            .get(cursor)
            .ok_or_else(|| pack_error("truncated object header"))?;
        let part = u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or_else(|| pack_error("object size overflow"))?;
        size = size
            .checked_add(part)
            .ok_or_else(|| pack_error("object size overflow"))?;
        shift = shift
            .checked_add(7)
            .ok_or_else(|| pack_error("object size overflow"))?;
        cursor += 1;
    }
    Ok((object_type, size, cursor))
}

fn parse_ofs_distance(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut byte = *data
        .get(*cursor)
        .ok_or_else(|| pack_error("missing OFS_DELTA base"))?;
    *cursor += 1;
    let mut distance = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *data
            .get(*cursor)
            .ok_or_else(|| pack_error("truncated OFS_DELTA base"))?;
        *cursor += 1;
        distance = distance
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
            .ok_or_else(|| pack_error("OFS_DELTA distance overflow"))?;
    }
    Ok(distance)
}

fn apply_delta(base: &[u8], delta: &[u8], max_size: usize) -> Result<Vec<u8>> {
    let mut cursor = 0;
    let base_size = delta_varint(delta, &mut cursor)?;
    if base_size != base.len() as u64 {
        return invalid("delta base size mismatch");
    }
    let result_size = delta_varint(delta, &mut cursor)?;
    if result_size > max_size as u64 {
        return Err(Error::ObjectTooLarge {
            declared: result_size,
            limit: max_size,
        });
    }
    let capacity = usize::try_from(result_size).map_err(|_| pack_error("delta size overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    while cursor < delta.len() {
        let opcode = delta[cursor];
        cursor += 1;
        if opcode & 0x80 != 0 {
            let mut offset = 0_usize;
            let mut size = 0_usize;
            for (bit, shift) in [(1, 0), (2, 8), (4, 16), (8, 24)] {
                if opcode & bit != 0 {
                    offset |= usize::from(take(delta, &mut cursor)?) << shift;
                }
            }
            for (bit, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if opcode & bit != 0 {
                    size |= usize::from(take(delta, &mut cursor)?) << shift;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset
                .checked_add(size)
                .ok_or_else(|| pack_error("delta copy overflow"))?;
            let source = base
                .get(offset..end)
                .ok_or_else(|| pack_error("delta copy outside base"))?;
            if output
                .len()
                .checked_add(size)
                .is_none_or(|length| length > capacity)
            {
                return invalid("delta output exceeds declared size");
            }
            output.extend_from_slice(source);
        } else if opcode != 0 {
            let length = usize::from(opcode);
            let end = cursor
                .checked_add(length)
                .ok_or_else(|| pack_error("delta insert overflow"))?;
            let literal = delta
                .get(cursor..end)
                .ok_or_else(|| pack_error("truncated delta insert"))?;
            if output
                .len()
                .checked_add(length)
                .is_none_or(|length| length > capacity)
            {
                return invalid("delta output exceeds declared size");
            }
            output.extend_from_slice(literal);
            cursor = end;
        } else {
            return invalid("reserved delta opcode zero");
        }
    }
    if output.len() != capacity {
        return invalid("delta output size mismatch");
    }
    Ok(output)
}

fn delta_varint(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = take(data, cursor)?;
        value |= u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or_else(|| pack_error("delta varint overflow"))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| pack_error("delta varint overflow"))?;
        if shift >= 64 {
            return invalid("delta varint overflow");
        }
    }
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, limit)
        .map_err(|error| Error::Compression(format!("{error:?}")))
}

fn kind_from_type(value: u8) -> Result<ObjectKind> {
    match value {
        1 => Ok(ObjectKind::Commit),
        2 => Ok(ObjectKind::Tree),
        3 => Ok(ObjectKind::Blob),
        4 => Ok(ObjectKind::Tag),
        _ => invalid("invalid packed object type"),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in data {
        let index = usize::from(((crc ^ u32::from(*byte)) & 0xff) as u8);
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    !crc
}

const CRC32_TABLE: [u32; 256] = make_crc32_table();

const fn make_crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0_u32;
    while index < 256 {
        let mut value = index;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 0 {
                value >> 1
            } else {
                (value >> 1) ^ 0xedb_88320
            };
            bit += 1;
        }
        table[index as usize] = value;
        index += 1;
    }
    table
}

fn be_u32(data: &[u8], offset: usize) -> Result<u32> {
    let end = checked_add(offset, 4)?;
    Ok(u32::from_be_bytes(
        data.get(offset..end)
            .ok_or_else(|| pack_error("truncated integer"))?
            .try_into()
            .unwrap(),
    ))
}
fn be_u64(data: &[u8], offset: usize) -> Result<u64> {
    let end = checked_add(offset, 8)?;
    Ok(u64::from_be_bytes(
        data.get(offset..end)
            .ok_or_else(|| pack_error("truncated integer"))?
            .try_into()
            .unwrap(),
    ))
}
fn read_hash(data: &[u8], offset: usize) -> Result<[u8; HASH_SIZE]> {
    let end = checked_add(offset, HASH_SIZE)?;
    data.get(offset..end)
        .ok_or_else(|| pack_error("truncated hash"))?
        .try_into()
        .map_err(|_| pack_error("invalid hash length"))
}
fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| pack_error("size overflow"))
}
fn checked_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right)
        .ok_or_else(|| pack_error("size overflow"))
}
fn take(data: &[u8], cursor: &mut usize) -> Result<u8> {
    let value = *data
        .get(*cursor)
        .ok_or_else(|| pack_error("truncated delta"))?;
    *cursor += 1;
    Ok(value)
}
fn object_label(id: ObjectId) -> PathBuf {
    Path::new("objects").join(id.to_string())
}
fn pack_error(message: &str) -> Error {
    Error::InvalidObject(format!("invalid pack: {message}"))
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(pack_error(message))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{PackIndex, apply_delta, crc32};
    use crate::object::sha1;
    use crate::{
        FileSystem, InitOptions, MemoryFileSystem, ObjectId, ObjectKind, PackOptions, Repository,
    };

    #[test]
    fn crc_matches_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn applies_copy_and_insert_delta() {
        let delta = [11, 10, 0x90, 6, 4, b'r', b'u', b's', b't'];
        assert_eq!(
            apply_delta(b"hello world", &delta, 10).unwrap(),
            b"hello rust"
        );
    }

    #[test]
    fn rejects_delta_writes_past_declared_result() {
        assert!(apply_delta(b"abc", &[3, 1, 2, b'x', b'y'], 10).is_err());
    }

    #[test]
    fn reads_a_git_compatible_indexed_pack_from_abstract_storage() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let contents = b"packed through an abstract filesystem";
        let id = ObjectId::compute(ObjectKind::Blob, contents);
        let size = u8::try_from(contents.len()).unwrap();
        let mut entry = vec![0xb0 | (size & 15), size >> 4];
        entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(contents, 6));

        let mut pack = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        pack.extend_from_slice(&entry);
        let pack_checksum = sha1::digest(&pack);
        pack.extend_from_slice(&pack_checksum);
        let index = one_object_index(id, crc32(&entry), 12, pack_checksum);
        fs.write(Path::new("repo/.git/objects/pack/fixture.pack"), &pack)
            .unwrap();
        fs.write(Path::new("repo/.git/objects/pack/fixture.idx"), &index)
            .unwrap();

        let object = repository.read_object(id, 1024).unwrap();
        assert_eq!(object.kind(), ObjectKind::Blob);
        assert_eq!(object.data(), contents);
        assert_eq!(PackIndex::parse(&index).unwrap().entries().len(), 1);
    }

    #[test]
    fn rejects_a_corrupt_pack_index_checksum() {
        let id = ObjectId::compute(ObjectKind::Blob, b"x");
        let mut index = one_object_index(id, 0, 12, [1; 20]);
        index[100] ^= 1;
        assert!(PackIndex::parse(&index).is_err());
    }

    #[test]
    fn builds_publishes_and_reads_a_delta_pack_in_memory() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let base_data = vec![b'a'; 4096];
        let mut target_data = base_data.clone();
        target_data[2048..2052].copy_from_slice(b"rust");
        let base = repository
            .write_object(ObjectKind::Blob, &base_data)
            .unwrap();
        let target = repository
            .write_object(ObjectKind::Blob, &target_data)
            .unwrap();

        let bundle = repository
            .build_pack(&[base, target, base], &PackOptions::default())
            .unwrap();
        assert_eq!(bundle.object_count(), 2);
        let index = PackIndex::parse(bundle.index()).unwrap();
        let target_offset = usize::try_from(index.find(target).unwrap().offset).unwrap();
        assert_eq!((bundle.pack()[target_offset] >> 4) & 7, 6);
        let written = repository
            .write_pack(&[base, target], &PackOptions::default())
            .unwrap();
        assert_eq!(written.object_count, 2);
        assert!(fs.exists(&written.pack_path).unwrap());
        assert!(fs.exists(&written.index_path).unwrap());

        remove_loose(&fs, base);
        remove_loose(&fs, target);
        let object = repository.read_object(target, 8192).unwrap();
        assert_eq!(object.kind(), ObjectKind::Blob);
        assert_eq!(object.data(), target_data);
    }

    #[test]
    fn builds_a_valid_empty_pack_and_index() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let bundle = repository.build_pack(&[], &PackOptions::default()).unwrap();
        assert_eq!(bundle.object_count(), 0);
        assert_eq!(PackIndex::parse(bundle.index()).unwrap().entries(), &[]);
    }

    fn one_object_index(id: ObjectId, crc: u32, offset: u32, pack_checksum: [u8; 20]) -> Vec<u8> {
        let mut index = Vec::new();
        index.extend_from_slice(&[0xff, b't', b'O', b'c']);
        index.extend_from_slice(&2_u32.to_be_bytes());
        for bucket in 0..256 {
            index.extend_from_slice(
                &u32::from(bucket >= usize::from(id.as_bytes()[0])).to_be_bytes(),
            );
        }
        index.extend_from_slice(id.as_bytes());
        index.extend_from_slice(&crc.to_be_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(&pack_checksum);
        let checksum = sha1::digest(&index);
        index.extend_from_slice(&checksum);
        index
    }

    fn remove_loose(fs: &MemoryFileSystem, id: ObjectId) {
        let hex = id.to_string();
        let path = Path::new("repo/.git/objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        fs.remove_file(&path).unwrap();
    }
}
