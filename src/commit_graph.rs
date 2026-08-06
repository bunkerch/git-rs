//! Git-compatible single-file commit-graph storage.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::object::sha1;
use crate::{Commit, Error, ObjectId, ObjectKind, Repository, Result};

const PATH: &str = "objects/info/commit-graph";
const NO_PARENT: u32 = 0x7000_0000;
const EXTRA_EDGE: u32 = 0x8000_0000;
const MAX_POSITION: usize = 0x6fff_ffff;
const MAX_GENERATION: u32 = 0x3fff_ffff;

/// Bounds and publication choices for commit-graph generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitGraphOptions {
    pub max_commits: usize,
    pub max_object_size: usize,
    pub max_file_size: usize,
    pub force: bool,
    pub dry_run: bool,
}

impl Default for CommitGraphOptions {
    fn default() -> Self {
        Self {
            max_commits: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
            max_file_size: 4 * 1024 * 1024 * 1024usize,
            force: false,
            dry_run: false,
        }
    }
}

/// Result of generating a commit graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitGraphReport {
    pub commits: usize,
    pub extra_edges: usize,
    pub bytes: usize,
    pub changed: bool,
}

/// Metadata for one commit in a parsed graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitGraphEntry {
    id: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    generation: u32,
    commit_time: u64,
}

impl CommitGraphEntry {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
    #[must_use]
    pub const fn tree(&self) -> ObjectId {
        self.tree
    }
    #[must_use]
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }
    #[must_use]
    pub const fn generation(&self) -> u32 {
        self.generation
    }
    #[must_use]
    pub const fn commit_time(&self) -> u64 {
        self.commit_time
    }
}

/// A validated, checksum-protected commit graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitGraph {
    entries: Vec<CommitGraphEntry>,
}

impl CommitGraph {
    /// Parse Git commit-graph version 1 with SHA-1 object IDs.
    ///
    /// # Errors
    /// Returns an error for unsupported, malformed, corrupt, or over-limit data.
    pub fn parse(data: &[u8], max_commits: usize) -> Result<Self> {
        parse_graph(data, max_commits)
    }
}

#[allow(clippy::too_many_lines)]
fn parse_graph(data: &[u8], max_commits: usize) -> Result<CommitGraph> {
    if data.len() < 8 + 4 * 12 + ObjectId::LENGTH || &data[..4] != b"CGPH" {
        return invalid("invalid or truncated header");
    }
    if data[4] != 1 || data[5] != 1 || data[7] != 0 {
        return invalid("unsupported version, hash, or base graph count");
    }
    let chunks = usize::from(data[6]);
    let table_end = 8usize
        .checked_add(
            (chunks + 1)
                .checked_mul(12)
                .ok_or_else(|| graph_error("chunk table overflow"))?,
        )
        .ok_or_else(|| graph_error("chunk table overflow"))?;
    if table_end > data.len().saturating_sub(ObjectId::LENGTH) {
        return invalid("truncated chunk table");
    }
    let payload_end = data.len() - ObjectId::LENGTH;
    if sha1::digest(&data[..payload_end]) != data[payload_end..] {
        return invalid("checksum mismatch");
    }
    let mut table = BTreeMap::new();
    let mut previous = table_end;
    for index in 0..=chunks {
        let at = 8 + index * 12;
        let id: [u8; 4] = data[at..at + 4].try_into().expect("bounded");
        let offset = usize::try_from(u64::from_be_bytes(
            data[at + 4..at + 12].try_into().expect("bounded"),
        ))
        .map_err(|_| graph_error("chunk offset overflow"))?;
        if offset < previous || offset > payload_end {
            return invalid("invalid chunk offsets");
        }
        if index == chunks {
            if id != [0; 4] || offset != payload_end {
                return invalid("invalid chunk terminator");
            }
        } else if id == [0; 4] || table.insert(id, (offset, 0usize)).is_some() {
            return invalid("duplicate or null chunk id");
        }
        if index > 0 {
            let prior_at = 8 + (index - 1) * 12;
            let prior: [u8; 4] = data[prior_at..prior_at + 4].try_into().expect("bounded");
            if let Some(value) = table.get_mut(&prior) {
                value.1 = offset;
            }
        }
        previous = offset;
    }
    let fanout = chunk(&table, data, *b"OIDF")?;
    if fanout.len() != 1024 {
        return invalid("OIDF has wrong size");
    }
    let mut last = 0usize;
    for word in fanout.chunks_exact(4) {
        let value = usize::try_from(u32::from_be_bytes(word.try_into().expect("four bytes")))
            .expect("u32 fits usize");
        if value < last {
            return invalid("OIDF is not monotonic");
        }
        last = value;
    }
    if last > max_commits || last > MAX_POSITION {
        return invalid("commit count exceeds limit");
    }
    let oids = chunk(&table, data, *b"OIDL")?;
    let cdat = chunk(&table, data, *b"CDAT")?;
    if oids.len() != last * 20 || cdat.len() != last * 36 {
        return invalid("OIDL or CDAT has wrong size");
    }
    if oids
        .chunks_exact(20)
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
    {
        return invalid("OIDs are not strictly sorted");
    }
    let ids = oids.chunks_exact(20).map(oid).collect::<Vec<_>>();
    let edges = table
        .get(b"EDGE")
        .map_or(&[][..], |&(start, end)| &data[start..end]);
    if edges.len() % 4 != 0 {
        return invalid("EDGE has wrong size");
    }
    let edge_words = edges
        .chunks_exact(4)
        .map(|v| u32::from_be_bytes(v.try_into().expect("four bytes")))
        .collect::<Vec<_>>();
    let mut entries = Vec::with_capacity(last);
    for (position, record) in cdat.chunks_exact(36).enumerate() {
        let tree = oid(&record[..20]);
        let first = be32(&record[20..24]);
        let second = be32(&record[24..28]);
        let mut parent_positions = Vec::new();
        if first != NO_PARENT {
            parent_positions.push(validate_parent(first, last, position)?);
        }
        if second & EXTRA_EDGE != 0 {
            let mut edge = usize::try_from(second & !EXTRA_EDGE).expect("u32 fits usize");
            loop {
                let value = *edge_words
                    .get(edge)
                    .ok_or_else(|| graph_error("EDGE pointer out of bounds"))?;
                parent_positions.push(validate_parent(value & !EXTRA_EDGE, last, position)?);
                edge += 1;
                if value & EXTRA_EDGE != 0 {
                    break;
                }
            }
        } else if second != NO_PARENT {
            parent_positions.push(validate_parent(second, last, position)?);
        }
        let packed = be32(&record[28..32]);
        let generation = packed >> 2;
        if generation == 0 {
            return invalid("zero generation number");
        }
        let commit_time = (u64::from(packed & 3) << 32) | u64::from(be32(&record[32..36]));
        entries.push(CommitGraphEntry {
            id: ids[position],
            tree,
            parents: parent_positions.into_iter().map(|p| ids[p]).collect(),
            generation,
            commit_time,
        });
    }
    Ok(CommitGraph { entries })
}

impl CommitGraph {
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[must_use]
    pub fn entries(&self) -> &[CommitGraphEntry] {
        &self.entries
    }
    #[must_use]
    pub fn get(&self, id: ObjectId) -> Option<&CommitGraphEntry> {
        self.entries
            .binary_search_by_key(&id, CommitGraphEntry::id)
            .ok()
            .map(|p| &self.entries[p])
    }

    /// Test ancestry using only graph positions and generation numbers.
    ///
    /// `None` means at least one endpoint is absent and the caller must inspect
    /// commit objects instead.
    ///
    /// # Errors
    /// Returns an error when traversal exceeds `max_commits`.
    pub fn is_ancestor(
        &self,
        ancestor: ObjectId,
        descendant: ObjectId,
        max_commits: usize,
    ) -> Result<Option<bool>> {
        let Some(ancestor_position) = self
            .entries
            .binary_search_by_key(&ancestor, CommitGraphEntry::id)
            .ok()
        else {
            return Ok(None);
        };
        let Some(descendant_position) = self
            .entries
            .binary_search_by_key(&descendant, CommitGraphEntry::id)
            .ok()
        else {
            return Ok(None);
        };
        let minimum_generation = self.entries[ancestor_position].generation;
        let mut pending = vec![descendant_position];
        let mut seen = HashSet::new();
        while let Some(position) = pending.pop() {
            if !seen.insert(position) {
                continue;
            }
            if seen.len() > max_commits {
                return invalid("ancestry walk exceeds commit limit");
            }
            if position == ancestor_position {
                return Ok(Some(true));
            }
            let entry = &self.entries[position];
            if entry.generation <= minimum_generation {
                continue;
            }
            for parent in entry.parents.iter().rev() {
                let parent_position = self
                    .entries
                    .binary_search_by_key(parent, CommitGraphEntry::id)
                    .map_err(|_| graph_error("graph parent is absent"))?;
                pending.push(parent_position);
            }
        }
        Ok(Some(false))
    }
}

impl Repository {
    /// Read and validate `objects/info/commit-graph` through the storage adapter.
    ///
    /// # Errors
    /// Returns an error for storage failures, malformed data, or exceeded limits.
    pub fn read_commit_graph(
        &self,
        max_file_size: usize,
        max_commits: usize,
    ) -> Result<CommitGraph> {
        let data = self.read_git_file(PATH)?;
        if data.len() > max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: data.len() as u64,
                limit: max_file_size,
            });
        }
        CommitGraph::parse(&data, max_commits)
    }

    /// Write a Git-compatible graph containing every commit reachable from `tips`.
    ///
    /// # Errors
    /// Returns an error for non-commit/missing objects, malformed history,
    /// exceeded limits, lock contention, or storage failures.
    pub fn write_commit_graph(
        &self,
        tips: &[ObjectId],
        options: &CommitGraphOptions,
    ) -> Result<CommitGraphReport> {
        let mut commits = HashMap::new();
        let mut pending = tips.to_vec();
        while let Some(id) = pending.pop() {
            if commits.contains_key(&id) {
                continue;
            }
            if commits.len() >= options.max_commits || commits.len() >= MAX_POSITION {
                return invalid("commit count exceeds limit");
            }
            let object = self.read_object_raw(id, options.max_object_size)?;
            if object.kind() != ObjectKind::Commit {
                return invalid("tip or parent is not a commit");
            }
            let commit = Commit::parse(object.data())?;
            pending.extend(commit.parents());
            commits.insert(id, commit);
        }
        let mut ids = commits.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let positions = ids
            .iter()
            .enumerate()
            .map(|(p, id)| {
                Ok((
                    *id,
                    u32::try_from(p).map_err(|_| graph_error("too many commits"))?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let mut generations = HashMap::new();
        let mut visiting = HashSet::new();
        for id in &ids {
            generation(*id, &commits, &mut generations, &mut visiting)?;
        }
        let (bytes, extra_edges) = encode(&ids, &positions, &commits, &generations)?;
        if bytes.len() > options.max_file_size {
            return Err(Error::ObjectTooLarge {
                declared: bytes.len() as u64,
                limit: options.max_file_size,
            });
        }
        let old = match self.read_git_file(PATH) {
            Ok(value) => Some(value),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let changed = options.force || old.as_deref() != Some(bytes.as_slice());
        if changed && !options.dry_run {
            self.write_atomic(Path::new(PATH), &bytes)?;
        }
        Ok(CommitGraphReport {
            commits: ids.len(),
            extra_edges,
            bytes: bytes.len(),
            changed,
        })
    }
}

fn encode(
    ids: &[ObjectId],
    positions: &HashMap<ObjectId, u32>,
    commits: &HashMap<ObjectId, Commit>,
    generations: &HashMap<ObjectId, u32>,
) -> Result<(Vec<u8>, usize)> {
    let mut fanout = [0u32; 256];
    for id in ids {
        for value in &mut fanout[usize::from(id.as_bytes()[0])..] {
            *value += 1;
        }
    }
    let extra_edges = commits
        .values()
        .map(|c| c.parents().len().saturating_sub(1))
        .filter(|n| *n > 1)
        .sum::<usize>();
    let chunk_count = if extra_edges == 0 { 3 } else { 4 };
    let header_len = 8 + (chunk_count + 1) * 12;
    let fanout_offset = header_len;
    let lookup_offset = fanout_offset + 1024;
    let cdat_at = lookup_offset + ids.len() * 20;
    let edge_at = cdat_at + ids.len() * 36;
    let end = edge_at + extra_edges * 4;
    let mut out = Vec::with_capacity(end + 20);
    out.extend_from_slice(b"CGPH\x01\x01");
    out.push(u8::try_from(chunk_count).expect("four"));
    out.push(0);
    for (id, offset) in [
        (*b"OIDF", fanout_offset),
        (*b"OIDL", lookup_offset),
        (*b"CDAT", cdat_at),
    ] {
        out.extend_from_slice(&id);
        out.extend_from_slice(&(offset as u64).to_be_bytes());
    }
    if extra_edges > 0 {
        out.extend_from_slice(b"EDGE");
        out.extend_from_slice(&(edge_at as u64).to_be_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(end as u64).to_be_bytes());
    for value in fanout {
        out.extend_from_slice(&value.to_be_bytes());
    }
    for id in ids {
        out.extend_from_slice(id.as_bytes());
    }
    let mut edge_words = Vec::with_capacity(extra_edges);
    for id in ids {
        let commit = &commits[id];
        out.extend_from_slice(commit.tree().as_bytes());
        let parents = commit.parents();
        out.extend_from_slice(
            &parents
                .first()
                .map_or(NO_PARENT, |p| positions[p])
                .to_be_bytes(),
        );
        if parents.len() <= 2 {
            out.extend_from_slice(
                &parents
                    .get(1)
                    .map_or(NO_PARENT, |p| positions[p])
                    .to_be_bytes(),
            );
        } else {
            let pointer = EXTRA_EDGE
                | u32::try_from(edge_words.len())
                    .map_err(|_| graph_error("too many extra edges"))?;
            out.extend_from_slice(&pointer.to_be_bytes());
            for (index, parent) in parents[1..].iter().enumerate() {
                let mut value = positions[parent];
                if index + 2 == parents.len() {
                    value |= EXTRA_EDGE;
                }
                edge_words.push(value);
            }
        }
        let timestamp = u64::try_from(commit.committer().timestamp())
            .map_err(|_| graph_error("negative commit timestamp"))?;
        if timestamp >= (1u64 << 34) {
            return invalid("commit timestamp exceeds 34-bit format");
        }
        let packed = (generations[id] << 2) | ((timestamp >> 32) as u32);
        out.extend_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes()[4..]);
    }
    for value in edge_words {
        out.extend_from_slice(&value.to_be_bytes());
    }
    let checksum = sha1::digest(&out);
    out.extend_from_slice(&checksum);
    Ok((out, extra_edges))
}

fn generation(
    id: ObjectId,
    commits: &HashMap<ObjectId, Commit>,
    done: &mut HashMap<ObjectId, u32>,
    visiting: &mut HashSet<ObjectId>,
) -> Result<u32> {
    if let Some(value) = done.get(&id) {
        return Ok(*value);
    }
    if !visiting.insert(id) {
        return invalid("commit graph contains a cycle");
    }
    let mut value = 1;
    for parent in commits[&id].parents() {
        value = value.max(
            generation(*parent, commits, done, visiting)?
                .saturating_add(1)
                .min(MAX_GENERATION),
        );
    }
    visiting.remove(&id);
    done.insert(id, value);
    Ok(value)
}

fn validate_parent(value: u32, count: usize, self_position: usize) -> Result<usize> {
    let position = usize::try_from(value).expect("u32 fits usize");
    if position >= count || position == self_position {
        return invalid("parent position out of bounds");
    }
    Ok(position)
}
fn chunk<'a>(
    table: &BTreeMap<[u8; 4], (usize, usize)>,
    data: &'a [u8],
    id: [u8; 4],
) -> Result<&'a [u8]> {
    let &(start, end) = table
        .get(&id)
        .ok_or_else(|| graph_error("required chunk missing"))?;
    Ok(&data[start..end])
}
fn oid(value: &[u8]) -> ObjectId {
    ObjectId::from_bytes(value.try_into().expect("twenty bytes"))
}
fn be32(value: &[u8]) -> u32 {
    u32::from_be_bytes(value.try_into().expect("four bytes"))
}
fn graph_error(message: &str) -> Error {
    Error::InvalidRepository(format!("invalid commit-graph: {message}"))
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(graph_error(message))
}

#[cfg(test)]
mod tests {
    use super::{CommitGraph, CommitGraphOptions};
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, ObjectId, ObjectKind, Repository, Signature,
    };

    fn commit(repo: &Repository, parents: &[ObjectId], timestamp: i64) -> ObjectId {
        let tree = repo.write_object(ObjectKind::Tree, b"").unwrap();
        let signature = Signature::new("A", "a@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repo.write_commit(&builder.build()).unwrap()
    }

    #[test]
    fn round_trips_roots_merges_and_octopus_edges() {
        let fs = MemoryFileSystem::new();
        let repo = Repository::init(fs, "repo", &InitOptions::default()).unwrap();
        let root = commit(&repo, &[], 1);
        let a = commit(&repo, &[root], 2);
        let b = commit(&repo, &[root], 3);
        let c = commit(&repo, &[root], 4);
        let tip = commit(&repo, &[a, b, c], 5);
        let report = repo
            .write_commit_graph(&[tip], &CommitGraphOptions::default())
            .unwrap();
        assert_eq!((report.commits, report.extra_edges), (5, 2));
        let graph = repo.read_commit_graph(1 << 20, 10).unwrap();
        let entry = graph.get(tip).unwrap();
        assert_eq!(entry.parents(), &[a, b, c]);
        assert_eq!(entry.generation(), 3);
        assert_eq!(entry.commit_time(), 5);
        assert_eq!(graph.is_ancestor(root, tip, 10).unwrap(), Some(true));
        assert_eq!(graph.is_ancestor(a, b, 10).unwrap(), Some(false));
        assert_eq!(graph.is_ancestor(ObjectId::null(), tip, 10).unwrap(), None);
        assert!(
            !repo
                .write_commit_graph(&[tip], &CommitGraphOptions::default())
                .unwrap()
                .changed
        );
    }

    #[test]
    fn rejects_corrupt_checksum_and_resource_limit() {
        let repo =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repo, &[], 1);
        repo.write_commit_graph(&[root], &CommitGraphOptions::default())
            .unwrap();
        let mut bytes = repo.read_git_file("objects/info/commit-graph").unwrap();
        bytes[20] ^= 1;
        assert!(CommitGraph::parse(&bytes, 10).is_err());
        let valid = repo.read_git_file("objects/info/commit-graph").unwrap();
        assert!(CommitGraph::parse(&valid, 0).is_err());
    }
}
