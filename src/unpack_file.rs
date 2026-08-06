//! Temporary materialization of blob objects over abstract storage.

use std::path::{Path, PathBuf};

use crate::{Error, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnpackFileOptions {
    pub max_object_size: usize,
    pub max_name_attempts: usize,
}

impl Default for UnpackFileOptions {
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024 * 1024,
            max_name_attempts: 1024,
        }
    }
}

/// An adapter-relative temporary blob file created by `unpack_file`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnpackedFile {
    path: PathBuf,
    len: usize,
}

impl UnpackedFile {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Repository {
    /// Materialize a loose or packed blob as a collision-exclusive temp file.
    ///
    /// The returned path belongs to the configured filesystem and is rooted in
    /// the worktree, or the Git directory for a bare repository.
    ///
    /// # Errors
    /// Returns an error for missing, corrupt, oversized, or non-blob objects,
    /// exhausted name attempts, or storage failures.
    pub fn unpack_file(&self, id: ObjectId, options: &UnpackFileOptions) -> Result<UnpackedFile> {
        let object = self.read_object(id, options.max_object_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject(format!("object {id} is not a blob")));
        }
        let root = self.work_tree().unwrap_or_else(|| self.git_dir());
        let hex = id.to_hex();
        let prefix = std::str::from_utf8(&hex[..12]).unwrap_or("000000000000");
        for attempt in 0..options.max_name_attempts {
            let path = root.join(format!(".merge_file_{prefix}_{attempt:08x}"));
            match self.filesystem().write_new(&path, object.data()) {
                Ok(()) => {
                    return Ok(UnpackedFile {
                        path,
                        len: object.data().len(),
                    });
                }
                Err(Error::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::InvalidRepository(
            "unpack-file temporary name attempts exhausted".into(),
        ))
    }

    /// Remove a temporary file previously returned by [`Self::unpack_file`].
    ///
    /// # Errors
    /// Returns an error when storage removal fails.
    pub fn remove_unpacked_file(&self, file: &UnpackedFile) -> Result<()> {
        self.filesystem().remove_file(&file.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem, PackOptions, Tree};

    #[test]
    fn materializes_loose_and_packed_blobs_with_collision_retries() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let id = repository
            .write_object(ObjectKind::Blob, b"contents")
            .unwrap();
        let first = repository
            .unpack_file(id, &UnpackFileOptions::default())
            .unwrap();
        let second = repository
            .unpack_file(id, &UnpackFileOptions::default())
            .unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(first.len(), 8);
        assert_eq!(filesystem.read(first.path()).unwrap(), b"contents");
        assert_eq!(filesystem.read(second.path()).unwrap(), b"contents");

        repository
            .write_pack(&[id], &PackOptions::default())
            .unwrap();
        filesystem
            .remove_file(&repository.git_path(format!(
                "objects/{}/{}",
                &id.to_string()[..2],
                &id.to_string()[2..]
            )))
            .unwrap();
        let packed = repository
            .unpack_file(id, &UnpackFileOptions::default())
            .unwrap();
        assert_eq!(filesystem.read(packed.path()).unwrap(), b"contents");
        repository.remove_unpacked_file(&packed).unwrap();
        assert!(!filesystem.exists(packed.path()).unwrap());
    }

    #[test]
    fn rejects_non_blobs_limits_and_exhausted_names() {
        let filesystem = MemoryFileSystem::new();
        let repository = Repository::init(filesystem, "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"large").unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        assert!(
            repository
                .unpack_file(tree, &UnpackFileOptions::default())
                .is_err()
        );
        assert!(
            repository
                .unpack_file(
                    blob,
                    &UnpackFileOptions {
                        max_object_size: 4,
                        ..UnpackFileOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            repository
                .unpack_file(
                    blob,
                    &UnpackFileOptions {
                        max_name_attempts: 0,
                        ..UnpackFileOptions::default()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn bare_repositories_materialize_below_the_git_root() {
        let filesystem = MemoryFileSystem::new();
        let repository = Repository::init(
            filesystem.clone(),
            "bare.git",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let id = repository.write_object(ObjectKind::Blob, b"").unwrap();
        let file = repository
            .unpack_file(id, &UnpackFileOptions::default())
            .unwrap();
        assert!(file.is_empty());
        assert!(file.path().starts_with("bare.git"));
        assert!(filesystem.exists(file.path()).unwrap());
    }
}
