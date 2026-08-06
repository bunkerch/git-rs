//! Validate, index, and publish incoming Git pack streams.

use crate::{IncomingPackOptions, ObjectId, Repository, Result, WrittenPack};

/// Integrity, publication, and marker policy for indexing an incoming pack.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexPackOptions {
    pub incoming: IncomingPackOptions,
    /// Parse structured objects and verify all direct links before publication.
    pub strict: bool,
    /// Validate and build the index without publishing any files.
    pub dry_run: bool,
    /// Create `pack-*.keep` before the index. A newline is appended when absent.
    pub keep_message: Option<Vec<u8>>,
    /// Create `pack-*.promisor` before the index with these byte-preserving contents.
    pub promisor_message: Option<Vec<u8>>,
}

/// Quarantine and publication metadata from an index-pack operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexPackReport {
    pub checksum: [u8; 20],
    pub objects: Vec<ObjectId>,
    pub pack_size: usize,
    pub index_size: usize,
    pub written: Option<WrittenPack>,
}

impl Repository {
    /// Validate an incoming pack, make thin input self-contained, build its v2
    /// index, and optionally publish it in the object database.
    ///
    /// # Errors
    /// Returns an error for corrupt framing/checksums/zlib/deltas, missing thin
    /// bases, exceeded limits, strict object/link failures, marker conflicts, or
    /// storage publication failures.
    pub fn index_pack(&self, pack: &[u8], options: &IndexPackOptions) -> Result<IndexPackReport> {
        let validated = self.validate_incoming_pack(pack, &options.incoming)?;
        if options.strict {
            crate::unpack_objects::validate_strict(
                self,
                &validated,
                options.incoming.max_object_size,
            )?;
        }
        let checksum = *validated.checksum();
        let objects = validated.object_ids().collect();
        let pack_size = validated.pack_size();
        let index_size = validated.index_size();
        let keep = options.keep_message.as_deref().map(marker_with_newline);
        let promisor = options.promisor_message.as_deref().map(marker_with_newline);
        let written = if options.dry_run {
            None
        } else {
            Some(self.publish_validated_pack_with_markers(
                &validated,
                keep.as_deref(),
                promisor.as_deref(),
            )?)
        };
        Ok(IndexPackReport {
            checksum,
            objects,
            pack_size,
            index_size,
            written,
        })
    }
}

fn marker_with_newline(contents: &[u8]) -> Vec<u8> {
    let mut marker = contents.to_vec();
    if !marker.is_empty() && !marker.ends_with(b"\n") {
        marker.push(b'\n');
    }
    marker
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        CommitBuilder, FileSystem, InitOptions, MemoryFileSystem, ObjectKind, PackOptions,
        Repository, Signature,
    };

    use super::IndexPackOptions;

    #[test]
    fn dry_run_builds_index_without_publication_and_markers_precede_visibility() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let first = source
            .write_object(ObjectKind::Blob, &[b'a'; 4096])
            .unwrap();
        let mut data = vec![b'a'; 4096];
        data[2000..2004].copy_from_slice(b"rust");
        let second = source.write_object(ObjectKind::Blob, &data).unwrap();
        let bundle = source
            .build_pack(&[first, second], &PackOptions::default())
            .unwrap();
        let fs = MemoryFileSystem::new();
        let destination =
            Repository::init(fs.clone(), "destination", &InitOptions::default()).unwrap();
        let dry = destination
            .index_pack(
                bundle.pack(),
                &IndexPackOptions {
                    dry_run: true,
                    ..IndexPackOptions::default()
                },
            )
            .unwrap();
        assert!(dry.written.is_none());
        assert_eq!(dry.objects.len(), 2);
        assert!(
            fs.read_dir(Path::new("destination/.git/objects/pack"))
                .unwrap()
                .is_empty()
        );

        let report = destination
            .index_pack(
                bundle.pack(),
                &IndexPackOptions {
                    keep_message: Some(b"fetch in progress".to_vec()),
                    promisor_message: Some(Vec::new()),
                    ..IndexPackOptions::default()
                },
            )
            .unwrap();
        let written = report.written.unwrap();
        assert_eq!(
            fs.read(&written.pack_path.with_extension("keep")).unwrap(),
            b"fetch in progress\n"
        );
        assert_eq!(
            fs.read(&written.pack_path.with_extension("promisor"))
                .unwrap(),
            b""
        );
        assert_eq!(destination.read_object(second, 8192).unwrap().data(), data);
    }

    #[test]
    fn strict_failure_keeps_pack_and_index_quarantined() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let missing_tree = crate::ObjectId::compute(ObjectKind::Tree, b"");
        let commit = CommitBuilder::new(
            missing_tree,
            Signature::new("A", "a@example.com", 1, 0).unwrap(),
            Signature::new("C", "c@example.com", 1, 0).unwrap(),
        )
        .message(b"broken\n".to_vec())
        .build();
        let id = source.write_commit(&commit).unwrap();
        let bundle = source.build_pack(&[id], &PackOptions::default()).unwrap();
        let fs = MemoryFileSystem::new();
        let destination =
            Repository::init(fs.clone(), "destination", &InitOptions::default()).unwrap();
        assert!(
            destination
                .index_pack(
                    bundle.pack(),
                    &IndexPackOptions {
                        strict: true,
                        ..IndexPackOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            fs.read_dir(Path::new("destination/.git/objects/pack"))
                .unwrap()
                .is_empty()
        );
    }
}
