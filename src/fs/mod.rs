//! Storage abstraction used by every repository operation.

mod host;
mod memory;
mod path;

use std::path::{Path, PathBuf};

pub use host::HostFileSystem;
pub use memory::MemoryFileSystem;

use crate::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    kind: FileType,
    len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileType {
    File,
    Directory,
}

impl Metadata {
    #[must_use]
    pub const fn file(len: u64) -> Self {
        Self {
            kind: FileType::File,
            len,
        }
    }

    #[must_use]
    pub const fn directory() -> Self {
        Self {
            kind: FileType::Directory,
            len: 0,
        }
    }

    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self.kind, FileType::File)
    }

    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self.kind, FileType::Directory)
    }

    #[must_use]
    pub const fn len(self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A repository-scoped filesystem.
///
/// Paths are always relative, normalized repository paths. Implementations must
/// reject absolute paths and `..` components. `write` replaces a complete file;
/// callers use `rename` to publish lock files atomically.
pub trait FileSystem: Send + Sync + 'static {
    /// Create a directory and all missing parents.
    ///
    /// # Errors
    /// Returns an error for an invalid path or if a component is a file.
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    /// Read a complete file.
    ///
    /// # Errors
    /// Returns an error when the path is invalid, absent, or not a file.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    /// Create or replace a complete file.
    ///
    /// # Errors
    /// Returns an error when the path is invalid or its parent is absent.
    fn write(&self, path: &Path, contents: &[u8]) -> Result<()>;
    /// Create a complete file only if it does not already exist.
    ///
    /// This is the lock acquisition primitive for repository transactions.
    ///
    /// # Errors
    /// Returns [`crate::Error::AlreadyExists`] if the path is occupied, or a
    /// storage error if the file cannot be created.
    fn write_new(&self, path: &Path, contents: &[u8]) -> Result<()>;
    /// Atomically move `from` to `to`, replacing a file at `to`.
    ///
    /// # Errors
    /// Returns an error when either path is invalid or the move cannot be completed.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Remove a file.
    ///
    /// # Errors
    /// Returns an error when the path is invalid, absent, or not a file.
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// Return metadata for a file or directory.
    ///
    /// # Errors
    /// Returns an error when the path is invalid or absent.
    fn metadata(&self, path: &Path) -> Result<Metadata>;
    /// Return the immediate child names of a directory in stable order.
    ///
    /// # Errors
    /// Returns an error when the path is invalid, absent, or not a directory.
    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Test whether a path exists.
    ///
    /// # Errors
    /// Returns storage errors other than an absent path.
    fn exists(&self, path: &Path) -> Result<bool> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(crate::Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}
