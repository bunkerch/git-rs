//! Deterministic TAR and ZIP archives generated from stored Git trees.

use crate::{
    AnnotatedTag, EntryMode, Error, ObjectId, ObjectKind, Repository, Result, RevisionOptions,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ArchiveFormat {
    #[default]
    Tar,
    Zip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveOptions {
    pub format: ArchiveFormat,
    /// A validated archive-relative prefix prepended to every entry.
    pub prefix: Vec<u8>,
    /// Literal repository-relative paths. Empty selects the complete tree.
    pub paths: Vec<Vec<u8>>,
    /// Override the commit timestamp (or zero for a tree object).
    pub timestamp: Option<u64>,
    pub max_object_size: usize,
    pub max_archive_size: usize,
    pub max_entries: usize,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            format: ArchiveFormat::Tar,
            prefix: Vec::new(),
            paths: Vec::new(),
            timestamp: None,
            max_object_size: 1024 * 1024 * 1024,
            max_archive_size: 4 * 1024 * 1024 * 1024,
            max_entries: 10_000_000,
        }
    }
}

struct ArchiveEntry {
    path: Vec<u8>,
    mode: EntryMode,
    data: Vec<u8>,
}

impl Repository {
    /// Archive a commit, tree, or annotated tag expression as TAR or ZIP bytes.
    ///
    /// # Errors
    /// Returns an error for a non-treeish revision, unsafe prefix/path, unmatched
    /// selection, corrupt object graph, or configured size/entry limit.
    pub fn archive(&self, revision: &str, options: &ArchiveOptions) -> Result<Vec<u8>> {
        let mut resolved = self.resolve_revision(
            revision,
            &RevisionOptions {
                max_object_size: options.max_object_size,
                ..RevisionOptions::default()
            },
        )?;
        for _ in 0..64 {
            if resolved.kind != ObjectKind::Tag {
                break;
            }
            let tag = AnnotatedTag::parse(
                self.read_object(resolved.id, options.max_object_size)?
                    .data(),
            )?;
            resolved.id = tag.target();
            resolved.kind = tag.target_kind();
        }
        let (tree, commit_time, commit_id) = match resolved.kind {
            ObjectKind::Commit => {
                let commit = self.read_commit(resolved.id, options.max_object_size)?;
                let timestamp = u64::try_from(commit.committer().timestamp()).map_err(|_| {
                    Error::InvalidCommit("archive timestamp is before the Unix epoch".into())
                })?;
                (commit.tree(), timestamp, Some(resolved.id))
            }
            ObjectKind::Tree => (resolved.id, 0, None),
            _ => {
                return Err(Error::InvalidRepository(
                    "archive requires a treeish object".into(),
                ));
            }
        };
        let prefix = normalize_archive_prefix(&options.prefix)?;
        let selections = options
            .paths
            .iter()
            .map(|path| normalize_archive_path(path))
            .collect::<Result<Vec<_>>>()?;
        let mut matched = vec![false; selections.len()];
        let mut entries = Vec::new();
        self.collect_archive_entries(tree, b"", &selections, &mut matched, options, &mut entries)?;
        if let Some((index, _)) = matched.iter().enumerate().find(|(_, found)| !**found) {
            return Err(Error::InvalidPath(bytes_path(&selections[index])));
        }
        for entry in &mut entries {
            if !prefix.is_empty() {
                let mut path = prefix.clone();
                path.extend_from_slice(&entry.path);
                entry.path = path;
            }
        }
        if !prefix.is_empty() {
            if entries.len() >= options.max_entries {
                return Err(Error::InvalidRepository(
                    "archive entry limit exceeded".into(),
                ));
            }
            entries.insert(
                0,
                ArchiveEntry {
                    path: prefix[..prefix.len() - 1].to_vec(),
                    mode: EntryMode::Tree,
                    data: Vec::new(),
                },
            );
        }
        let timestamp = options.timestamp.unwrap_or(commit_time);
        match options.format {
            ArchiveFormat::Tar => {
                encode_tar(&entries, timestamp, commit_id, options.max_archive_size)
            }
            ArchiveFormat::Zip => {
                encode_zip(&entries, timestamp, commit_id, options.max_archive_size)
            }
        }
    }

    fn collect_archive_entries(
        &self,
        tree_id: ObjectId,
        base: &[u8],
        selections: &[Vec<u8>],
        matched: &mut [bool],
        options: &ArchiveOptions,
        output: &mut Vec<ArchiveEntry>,
    ) -> Result<()> {
        let tree = self.read_tree(tree_id, options.max_object_size)?;
        for item in tree.entries() {
            let mut path = base.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(item.name());
            let selected =
                selections.is_empty() || selections.iter().any(|value| path_selected(&path, value));
            let descendant_selected = selections.iter().any(|value| is_path_prefix(&path, value));
            if !selected && !descendant_selected {
                continue;
            }
            for (index, selection) in selections.iter().enumerate() {
                if path_selected(&path, selection) || is_path_prefix(selection, &path) {
                    matched[index] = true;
                }
            }
            match item.mode() {
                EntryMode::Tree => {
                    push_archive_entry(
                        output,
                        ArchiveEntry {
                            path: path.clone(),
                            mode: EntryMode::Tree,
                            data: Vec::new(),
                        },
                        options.max_entries,
                    )?;
                    self.collect_archive_entries(
                        item.id(),
                        &path,
                        selections,
                        matched,
                        options,
                        output,
                    )?;
                }
                EntryMode::Gitlink => push_archive_entry(
                    output,
                    ArchiveEntry {
                        path,
                        mode: EntryMode::Gitlink,
                        data: Vec::new(),
                    },
                    options.max_entries,
                )?,
                mode => {
                    let object = self.read_object(item.id(), options.max_object_size)?;
                    if object.kind() != ObjectKind::Blob {
                        return Err(Error::InvalidObject(format!(
                            "archive entry {} is not a blob",
                            item.id()
                        )));
                    }
                    push_archive_entry(
                        output,
                        ArchiveEntry {
                            path,
                            mode,
                            data: object.into_data(),
                        },
                        options.max_entries,
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn push_archive_entry(
    output: &mut Vec<ArchiveEntry>,
    entry: ArchiveEntry,
    limit: usize,
) -> Result<()> {
    if output.len() >= limit {
        return Err(Error::InvalidRepository(
            "archive entry limit exceeded".into(),
        ));
    }
    output.push(entry);
    Ok(())
}

fn normalize_archive_prefix(value: &[u8]) -> Result<Vec<u8>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut path = normalize_archive_path(value)?;
    path.push(b'/');
    Ok(path)
}

fn normalize_archive_path(value: &[u8]) -> Result<Vec<u8>> {
    if value.is_empty() || value[0] == b'/' || value.contains(&0) {
        return Err(Error::InvalidPath(bytes_path(value)));
    }
    let mut components = Vec::new();
    for component in value.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => return Err(Error::InvalidPath(bytes_path(value))),
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return Err(Error::InvalidPath(bytes_path(value)));
    }
    Ok(components.join(&b'/'))
}

fn bytes_path(value: &[u8]) -> std::path::PathBuf {
    String::from_utf8_lossy(value).into_owned().into()
}

fn path_selected(path: &[u8], selection: &[u8]) -> bool {
    path == selection || is_path_prefix(selection, path)
}

fn is_path_prefix(prefix: &[u8], path: &[u8]) -> bool {
    path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/')
}

fn entry_path(entry: &ArchiveEntry) -> Vec<u8> {
    let mut path = entry.path.clone();
    if matches!(entry.mode, EntryMode::Tree | EntryMode::Gitlink) {
        path.push(b'/');
    }
    path
}

fn encode_tar(
    entries: &[ArchiveEntry],
    timestamp: u64,
    commit: Option<ObjectId>,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(id) = commit {
        let comment = pax_record(b"comment", &id.to_hex());
        tar_append(
            &mut output,
            b"pax_global_header",
            0o664,
            timestamp,
            b'g',
            None,
            &comment,
            limit,
        )?;
    }
    for (index, entry) in entries.iter().enumerate() {
        let path = entry_path(entry);
        let link = (entry.mode == EntryMode::Link).then_some(entry.data.as_slice());
        let mut pax = Vec::new();
        if !tar_path_fits(&path) {
            pax.extend(pax_record(b"path", &path));
        }
        if link.is_some_and(|value| value.len() > 100) {
            pax.extend(pax_record(b"linkpath", link.unwrap()));
        }
        if !pax.is_empty() {
            let name = format!("PaxHeaders/{index}");
            tar_append(
                &mut output,
                name.as_bytes(),
                0o664,
                timestamp,
                b'x',
                None,
                &pax,
                limit,
            )?;
        }
        let placeholder = if tar_path_fits(&path) {
            path.as_slice()
        } else {
            b"PaxHeaders/data"
        };
        let (mode, kind, data) = match entry.mode {
            EntryMode::Tree | EntryMode::Gitlink => (0o775, b'5', &[][..]),
            EntryMode::Link => (0o777, b'2', &[][..]),
            EntryMode::BlobExecutable => (0o775, b'0', entry.data.as_slice()),
            EntryMode::Blob => (0o664, b'0', entry.data.as_slice()),
        };
        tar_append(
            &mut output,
            placeholder,
            mode,
            timestamp,
            kind,
            link,
            data,
            limit,
        )?;
    }
    append_limited(&mut output, &[0; 1024], limit)?;
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn tar_append(
    output: &mut Vec<u8>,
    path: &[u8],
    mode: u64,
    timestamp: u64,
    kind: u8,
    link: Option<&[u8]>,
    data: &[u8],
    limit: usize,
) -> Result<()> {
    let mut header = [0_u8; 512];
    tar_set_path(&mut header, path)?;
    tar_octal(&mut header[100..108], mode)?;
    tar_octal(&mut header[108..116], 0)?;
    tar_octal(&mut header[116..124], 0)?;
    tar_octal(
        &mut header[124..136],
        u64::try_from(data.len()).map_err(|_| archive_too_large())?,
    )?;
    tar_octal(&mut header[136..148], timestamp)?;
    header[148..156].fill(b' ');
    header[156] = kind;
    if let Some(value) = link {
        header[157..157 + value.len().min(100)].copy_from_slice(&value[..value.len().min(100)]);
    }
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[265..269].copy_from_slice(b"root");
    header[297..301].copy_from_slice(b"root");
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    let text = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(text.as_bytes());
    append_limited(output, &header, limit)?;
    append_limited(output, data, limit)?;
    let padding = (512 - data.len() % 512) % 512;
    append_limited(output, &vec![0; padding], limit)
}

fn tar_path_fits(path: &[u8]) -> bool {
    path.len() <= 100 || path.len() <= 255 && path[..path.len().saturating_sub(100)].contains(&b'/')
}

fn tar_set_path(header: &mut [u8; 512], path: &[u8]) -> Result<()> {
    if path.len() <= 100 {
        header[..path.len()].copy_from_slice(path);
        return Ok(());
    }
    let split = (1..path.len())
        .rev()
        .find(|index| path[*index] == b'/' && *index <= 155 && path.len() - index - 1 <= 100)
        .ok_or_else(|| {
            Error::InvalidRepository("TAR path does not fit after PAX substitution".into())
        })?;
    header[..path.len() - split - 1].copy_from_slice(&path[split + 1..]);
    header[345..345 + split].copy_from_slice(&path[..split]);
    Ok(())
}

fn tar_octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = format!("{value:o}");
    if digits.len() + 1 > field.len() {
        return Err(archive_too_large());
    }
    field.fill(b'0');
    let start = field.len() - digits.len() - 1;
    field[start..start + digits.len()].copy_from_slice(digits.as_bytes());
    field[field.len() - 1] = 0;
    Ok(())
}

fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let base = 1 + key.len() + 1 + value.len() + 1;
    let mut length = base + decimal_digits(base);
    loop {
        let adjusted = base + decimal_digits(length);
        if adjusted == length {
            break;
        }
        length = adjusted;
    }
    let mut record = format!("{length} ").into_bytes();
    record.extend_from_slice(key);
    record.push(b'=');
    record.extend_from_slice(value);
    record.push(b'\n');
    record
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

struct ZipCentral {
    path: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
    flags: u16,
    mode: u32,
    extra: Vec<u8>,
}

#[allow(clippy::too_many_lines)]
fn encode_zip(
    entries: &[ArchiveEntry],
    timestamp: u64,
    commit: Option<ObjectId>,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut central = Vec::with_capacity(entries.len());
    let unix_time = u32::try_from(timestamp).unwrap_or(u32::MAX);
    let extra = zip_time_extra(unix_time);
    for entry in entries {
        let path = entry_path(entry);
        if path.len() > usize::from(u16::MAX) {
            return Err(archive_too_large());
        }
        let data = if matches!(entry.mode, EntryMode::Tree | EntryMode::Gitlink) {
            &[][..]
        } else {
            entry.data.as_slice()
        };
        let size = u32::try_from(data.len()).map_err(|_| archive_too_large())?;
        let path_length = u16::try_from(path.len()).map_err(|_| archive_too_large())?;
        let extra_length = u16::try_from(extra.len()).map_err(|_| archive_too_large())?;
        let offset = u32::try_from(output.len()).map_err(|_| archive_too_large())?;
        let crc = crc32(data);
        let flags = if std::str::from_utf8(&path).is_ok() && !path.is_ascii() {
            0x0800
        } else {
            0
        };
        put_u32(&mut output, 0x0403_4b50);
        put_u16(&mut output, 20);
        put_u16(&mut output, flags);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0x21);
        put_u32(&mut output, crc);
        put_u32(&mut output, size);
        put_u32(&mut output, size);
        put_u16(&mut output, path_length);
        put_u16(&mut output, extra_length);
        append_limited(&mut output, &path, limit)?;
        append_limited(&mut output, &extra, limit)?;
        append_limited(&mut output, data, limit)?;
        let mode = match entry.mode {
            EntryMode::Tree | EntryMode::Gitlink => 0o040_775,
            EntryMode::Link => 0o120_777,
            EntryMode::BlobExecutable => 0o100_775,
            EntryMode::Blob => 0o100_664,
        };
        central.push(ZipCentral {
            path,
            crc,
            size,
            offset,
            flags,
            mode,
            extra: extra.clone(),
        });
        if output.len() > limit {
            return Err(archive_too_large());
        }
    }
    let central_offset = u32::try_from(output.len()).map_err(|_| archive_too_large())?;
    for item in &central {
        put_u32(&mut output, 0x0201_4b50);
        put_u16(&mut output, 0x0314);
        put_u16(&mut output, 20);
        put_u16(&mut output, item.flags);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0x21);
        put_u32(&mut output, item.crc);
        put_u32(&mut output, item.size);
        put_u32(&mut output, item.size);
        put_u16(
            &mut output,
            u16::try_from(item.path.len()).map_err(|_| archive_too_large())?,
        );
        put_u16(
            &mut output,
            u16::try_from(item.extra.len()).map_err(|_| archive_too_large())?,
        );
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u32(
            &mut output,
            (item.mode << 16) | (u32::from(item.path.ends_with(b"/")) * 0x10),
        );
        put_u32(&mut output, item.offset);
        append_limited(&mut output, &item.path, limit)?;
        append_limited(&mut output, &item.extra, limit)?;
    }
    let central_size =
        u32::try_from(output.len()).map_err(|_| archive_too_large())? - central_offset;
    let count = u16::try_from(central.len()).map_err(|_| archive_too_large())?;
    put_u32(&mut output, 0x0605_4b50);
    let comment = commit.map(ObjectId::to_hex);
    put_u16(&mut output, 0);
    put_u16(&mut output, 0);
    put_u16(&mut output, count);
    put_u16(&mut output, count);
    put_u32(&mut output, central_size);
    put_u32(&mut output, central_offset);
    put_u16(
        &mut output,
        if comment.is_some() {
            u16::try_from(ObjectId::HEX_LENGTH).expect("object ID fits ZIP comment")
        } else {
            0
        },
    );
    if let Some(comment) = comment {
        append_limited(&mut output, &comment, limit)?;
    }
    if output.len() > limit {
        return Err(archive_too_large());
    }
    Ok(output)
}

fn zip_time_extra(timestamp: u32) -> Vec<u8> {
    let mut value = Vec::with_capacity(9);
    put_u16(&mut value, 0x5455);
    put_u16(&mut value, 5);
    value.push(1);
    put_u32(&mut value, timestamp);
    value
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn append_limited(output: &mut Vec<u8>, data: &[u8], limit: usize) -> Result<()> {
    if output
        .len()
        .checked_add(data.len())
        .is_none_or(|size| size > limit)
    {
        return Err(archive_too_large());
    }
    output.extend_from_slice(data);
    Ok(())
}

fn archive_too_large() -> Error {
    Error::InvalidRepository("archive size limit exceeded".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, FileSystem, InitOptions, MemoryFileSystem, Signature};
    use std::path::Path;

    type TarHeader = (Vec<u8>, u8, u64, Vec<u8>, Vec<u8>);

    fn fixture() -> (Repository, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem.create_dir_all(Path::new("repo/dir")).unwrap();
        filesystem
            .write(Path::new("repo/plain"), b"plain\n")
            .unwrap();
        filesystem
            .write(Path::new("repo/run"), b"#!/bin/sh\n")
            .unwrap();
        filesystem
            .set_executable(Path::new("repo/run"), true)
            .unwrap();
        filesystem
            .create_symlink(Path::new("repo/link"), b"plain")
            .unwrap();
        filesystem
            .write(Path::new("repo/dir/nested"), b"nested\n")
            .unwrap();
        repository.add(".").unwrap();
        let signature = Signature::new("Archive", "archive@example.com", 1_700_000_000, 0).unwrap();
        let commit = repository
            .commit_index(
                b"archive",
                &signature,
                &signature,
                &CommitOptions::default(),
            )
            .unwrap();
        (repository, commit)
    }

    #[test]
    fn tar_preserves_paths_content_modes_and_symlink_targets() {
        let (repository, commit) = fixture();
        let archive = repository
            .archive(
                &commit.to_string(),
                &ArchiveOptions {
                    prefix: b"source".to_vec(),
                    ..ArchiveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(archive.len() % 512, 0);
        assert!(archive.ends_with(&[0; 1024]));
        let headers = tar_headers(&archive);
        assert!(
            headers
                .iter()
                .any(|(path, kind, mode, data, _)| path == b"source/plain"
                    && *kind == b'0'
                    && *mode == 0o664
                    && data == b"plain\n")
        );
        assert!(
            headers
                .iter()
                .any(|(path, kind, mode, _, _)| path == b"source/run"
                    && *kind == b'0'
                    && *mode == 0o775)
        );
        assert!(
            headers
                .iter()
                .any(|(path, kind, mode, _, link)| path == b"source/link"
                    && *kind == b'2'
                    && *mode == 0o777
                    && link == b"plain")
        );
        assert!(
            headers
                .iter()
                .any(|(path, kind, _, _, _)| path == b"source/dir/" && *kind == b'5')
        );
    }

    #[test]
    fn zip_contains_stored_entries_central_directory_and_unix_modes() {
        let (repository, commit) = fixture();
        let archive = repository
            .archive(
                &commit.to_string(),
                &ArchiveOptions {
                    format: ArchiveFormat::Zip,
                    ..ArchiveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(&archive[..4], &0x0403_4b50_u32.to_le_bytes());
        assert!(
            archive
                .windows(4)
                .any(|value| value == 0x0201_4b50_u32.to_le_bytes())
        );
        assert!(
            archive
                .windows(4)
                .any(|value| value == 0x0605_4b50_u32.to_le_bytes())
        );
        assert!(archive.ends_with(&commit.to_hex()));
        for name in [b"dir/".as_slice(), b"dir/nested", b"link", b"plain", b"run"] {
            assert!(archive.windows(name.len()).any(|value| value == name));
        }
        assert!(
            archive
                .windows(b"nested\n".len())
                .any(|value| value == b"nested\n")
        );
    }

    #[test]
    fn literal_selection_long_paths_and_limits_are_enforced() {
        let (repository, commit) = fixture();
        let selected = repository
            .archive(
                &commit.to_string(),
                &ArchiveOptions {
                    paths: vec![b"dir/nested".to_vec()],
                    ..ArchiveOptions::default()
                },
            )
            .unwrap();
        assert!(selected.windows(10).any(|value| value == b"dir/nested"));
        assert!(!selected.windows(5).any(|value| value == b"plain"));
        assert!(
            repository
                .archive(
                    &commit.to_string(),
                    &ArchiveOptions {
                        paths: vec![b"missing".to_vec()],
                        ..ArchiveOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            repository
                .archive(
                    &commit.to_string(),
                    &ArchiveOptions {
                        max_archive_size: 512,
                        ..ArchiveOptions::default()
                    }
                )
                .is_err()
        );

        let long_name = vec![b'a'; 140];
        let blob = repository.write_object(ObjectKind::Blob, b"long").unwrap();
        let tree = crate::Tree::new(vec![
            crate::TreeEntry::new(EntryMode::Blob, long_name.clone(), blob).unwrap(),
        ])
        .unwrap();
        let tree = repository.write_tree(&tree).unwrap();
        let archive = repository
            .archive(&tree.to_string(), &ArchiveOptions::default())
            .unwrap();
        let mut marker = b"path=".to_vec();
        marker.extend_from_slice(&long_name);
        assert!(archive.windows(marker.len()).any(|value| value == marker));
    }

    #[test]
    fn annotated_tags_and_non_utf8_names_archive_without_loss() {
        let (repository, commit) = fixture();
        let signature = Signature::new("Tag", "tag@example.com", 1_700_000_001, 0).unwrap();
        let tag = crate::TagBuilder::new(commit, ObjectKind::Commit, b"release", signature)
            .unwrap()
            .message(b"release".to_vec())
            .build();
        repository
            .create_annotated_tag("release", &tag, false, 1024 * 1024)
            .unwrap();
        let tagged = repository
            .archive("release", &ArchiveOptions::default())
            .unwrap();
        assert!(tagged.windows(5).any(|value| value == b"plain"));

        let name = b"invalid-\xff-name".to_vec();
        let blob = repository.write_object(ObjectKind::Blob, b"bytes").unwrap();
        let tree = crate::Tree::new(vec![
            crate::TreeEntry::new(EntryMode::Blob, name.clone(), blob).unwrap(),
        ])
        .unwrap();
        let tree = repository.write_tree(&tree).unwrap();
        for format in [ArchiveFormat::Tar, ArchiveFormat::Zip] {
            let archive = repository
                .archive(
                    &tree.to_string(),
                    &ArchiveOptions {
                        format,
                        ..ArchiveOptions::default()
                    },
                )
                .unwrap();
            assert!(archive.windows(name.len()).any(|value| value == name));
        }
    }

    fn tar_headers(archive: &[u8]) -> Vec<TarHeader> {
        let mut result = Vec::new();
        let mut offset = 0;
        while offset + 512 <= archive.len()
            && archive[offset..offset + 512].iter().any(|byte| *byte != 0)
        {
            let header = &archive[offset..offset + 512];
            let mut path = header[..100]
                .split(|byte| *byte == 0)
                .next()
                .unwrap()
                .to_vec();
            let prefix = header[345..500].split(|byte| *byte == 0).next().unwrap();
            if !prefix.is_empty() {
                let mut joined = prefix.to_vec();
                joined.push(b'/');
                joined.extend(path);
                path = joined;
            }
            let mode = parse_octal(&header[100..108]);
            let size = usize::try_from(parse_octal(&header[124..136])).unwrap();
            let link = header[157..257]
                .split(|byte| *byte == 0)
                .next()
                .unwrap()
                .to_vec();
            offset += 512;
            let data = archive[offset..offset + size].to_vec();
            result.push((path, header[156], mode, data, link));
            offset += size.div_ceil(512) * 512;
        }
        result
    }

    fn parse_octal(value: &[u8]) -> u64 {
        value
            .iter()
            .copied()
            .filter(u8::is_ascii_digit)
            .fold(0, |number, byte| number * 8 + u64::from(byte - b'0'))
    }
}
