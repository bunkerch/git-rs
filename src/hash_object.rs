//! Compute and optionally store Git objects.

use std::path::{Path, PathBuf};

use crate::{AnnotatedTag, Commit, Error, ObjectId, ObjectKind, Repository, Result, Tree};

/// Controls hashing, validation, storage, and input resource limits.
#[derive(Clone, Debug)]
pub struct HashObjectOptions {
    /// Canonical Git object type.
    pub kind: ObjectKind,
    /// Publish the resulting loose object in the object database.
    pub write: bool,
    /// Skip structural validation, matching Git's `--literally` option.
    pub literally: bool,
    /// Maximum number of content bytes accepted for one object.
    pub max_input_size: usize,
}

impl Default for HashObjectOptions {
    fn default() -> Self {
        Self {
            kind: ObjectKind::Blob,
            write: false,
            literally: false,
            max_input_size: 512 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Hash bytes using Git's `<type> <size>\0<contents>` object framing.
    ///
    /// When `write` is enabled, the zlib-compressed loose object is atomically
    /// published through the configured [`crate::FileSystem`].
    ///
    /// # Errors
    /// Returns an error when the input exceeds its limit, typed contents are
    /// malformed, or loose-object publication fails.
    pub fn hash_object(&self, contents: &[u8], options: &HashObjectOptions) -> Result<ObjectId> {
        enforce_limit(contents.len(), options.max_input_size)?;
        if !options.literally {
            validate_contents(options.kind, contents)?;
        }
        if options.write {
            self.write_object(options.kind, contents)
        } else {
            Ok(ObjectId::compute(options.kind, contents))
        }
    }

    /// Read and hash one worktree-relative path through the filesystem adapter.
    ///
    /// # Errors
    /// Returns an error for bare repositories, unsafe or missing paths, inputs
    /// over the byte limit, malformed typed contents, or storage failures.
    pub fn hash_object_path(
        &self,
        path: impl AsRef<Path>,
        options: &HashObjectOptions,
    ) -> Result<ObjectId> {
        let relative = validate_relative_path(path.as_ref())?;
        let root = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("hashing paths requires a working tree".into())
        })?;
        let storage_path = root.join(relative);
        let declared = self.filesystem().metadata(&storage_path)?.len();
        if declared > options.max_input_size as u64 {
            return Err(Error::ObjectTooLarge {
                declared,
                limit: options.max_input_size,
            });
        }
        let contents = self.filesystem().read(&storage_path)?;
        self.hash_object(&contents, options)
    }

    /// Hash a bounded list of worktree-relative paths in input order.
    ///
    /// # Errors
    /// Returns an error when the path-count limit is exceeded or any individual
    /// path fails under [`Repository::hash_object_path`].
    pub fn hash_object_paths<I, P>(
        &self,
        paths: I,
        options: &HashObjectOptions,
        max_paths: usize,
    ) -> Result<Vec<ObjectId>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut result = Vec::new();
        for path in paths {
            if result.len() == max_paths {
                return Err(Error::Protocol(format!(
                    "hash-object path count exceeds limit {max_paths}"
                )));
            }
            result.push(self.hash_object_path(path, options)?);
        }
        Ok(result)
    }
}

fn enforce_limit(actual: usize, limit: usize) -> Result<()> {
    if actual > limit {
        return Err(Error::ObjectTooLarge {
            declared: actual as u64,
            limit,
        });
    }
    Ok(())
}

fn validate_contents(kind: ObjectKind, contents: &[u8]) -> Result<()> {
    match kind {
        ObjectKind::Blob => {}
        ObjectKind::Tree => drop(Tree::parse(contents)?),
        ObjectKind::Commit => drop(Commit::parse(contents)?),
        ObjectKind::Tag => drop(AnnotatedTag::parse(contents)?),
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<PathBuf> {
    crate::fs::normalize_path(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::HashObjectOptions;
    use crate::{Error, FileSystem, InitOptions, MemoryFileSystem, ObjectKind, Repository};

    #[test]
    fn hashes_without_writing_and_writes_on_request() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let options = HashObjectOptions::default();
        let id = repository.hash_object(b"hello\n", &options).unwrap();
        assert_eq!(id.to_string(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert!(!repository.contains_loose_object(id).unwrap());
        let written = repository
            .hash_object(
                b"hello\n",
                &HashObjectOptions {
                    write: true,
                    ..options
                },
            )
            .unwrap();
        assert_eq!(written, id);
        assert!(repository.contains_loose_object(id).unwrap());
        assert_eq!(
            repository
                .hash_object(
                    b"",
                    &HashObjectOptions {
                        kind: ObjectKind::Tree,
                        ..HashObjectOptions::default()
                    }
                )
                .unwrap()
                .to_string(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn hashes_adapter_paths_and_preserves_order() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(Path::new("repo/a"), b"a").unwrap();
        fs.write(Path::new("repo/b"), b"b").unwrap();
        let ids = repository
            .hash_object_paths(["a", "b"], &HashObjectOptions::default(), 2)
            .unwrap();
        assert_eq!(ids[0], crate::ObjectId::compute(ObjectKind::Blob, b"a"));
        assert_eq!(ids[1], crate::ObjectId::compute(ObjectKind::Blob, b"b"));
    }

    #[test]
    fn enforces_byte_and_path_limits() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let error = repository
            .hash_object(
                b"large",
                &HashObjectOptions {
                    max_input_size: 4,
                    ..HashObjectOptions::default()
                },
            )
            .unwrap_err();
        assert!(matches!(error, Error::ObjectTooLarge { .. }));
        assert!(
            repository
                .hash_object_paths(["a"], &HashObjectOptions::default(), 0)
                .is_err()
        );
    }

    #[test]
    fn literal_mode_hashes_and_stores_malformed_canonical_objects() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let id = repository
            .hash_object(
                b"garbage",
                &HashObjectOptions {
                    kind: ObjectKind::Commit,
                    write: true,
                    literally: true,
                    ..HashObjectOptions::default()
                },
            )
            .unwrap();
        assert_eq!(id.to_string(), "4fa7f4e4a001931b2461ca12ef78ce3c5cac8e27");
        assert!(repository.contains_loose_object(id).unwrap());
        assert_eq!(
            repository.read_object(id, 1024).unwrap().kind(),
            ObjectKind::Commit
        );
        assert!(
            repository
                .hash_object(
                    b"garbage",
                    &HashObjectOptions {
                        kind: ObjectKind::Commit,
                        ..HashObjectOptions::default()
                    }
                )
                .is_err()
        );
    }
}
