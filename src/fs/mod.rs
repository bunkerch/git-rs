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
    executable: bool,
    stat: FileStat,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileStat {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub device: u32,
    pub inode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl FileStat {
    pub const EMPTY: Self = Self {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        device: 0,
        inode: 0,
        uid: 0,
        gid: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileType {
    File,
    Directory,
    Symlink,
}

impl Metadata {
    #[must_use]
    pub const fn file(len: u64) -> Self {
        Self {
            kind: FileType::File,
            len,
            executable: false,
            stat: FileStat::EMPTY,
        }
    }

    #[must_use]
    pub const fn directory() -> Self {
        Self {
            kind: FileType::Directory,
            len: 0,
            executable: false,
            stat: FileStat::EMPTY,
        }
    }

    #[must_use]
    pub const fn symlink(len: u64) -> Self {
        Self {
            kind: FileType::Symlink,
            len,
            executable: false,
            stat: FileStat::EMPTY,
        }
    }

    #[must_use]
    pub const fn with_executable(mut self, executable: bool) -> Self {
        self.executable = executable;
        self
    }

    #[must_use]
    pub const fn with_stat(mut self, stat: FileStat) -> Self {
        self.stat = stat;
        self
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
    pub const fn is_symlink(self) -> bool {
        matches!(self.kind, FileType::Symlink)
    }

    #[must_use]
    pub const fn is_executable(self) -> bool {
        self.executable
    }

    #[must_use]
    pub const fn stat(self) -> FileStat {
        self.stat
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
    /// Read the target bytes of a symbolic link without following it.
    ///
    /// # Errors
    /// Returns an error when the path is absent or not a symbolic link.
    fn read_link(&self, path: &Path) -> Result<Vec<u8>>;
    /// Create or replace a symbolic link with byte-preserving target data.
    ///
    /// # Errors
    /// Returns an error for invalid paths, unsupported target bytes, or storage failure.
    fn create_symlink(&self, path: &Path, target: &[u8]) -> Result<()>;
    /// Change a regular file's executable bit.
    ///
    /// # Errors
    /// Returns an error when the path is not a regular file or storage fails.
    fn set_executable(&self, path: &Path, executable: bool) -> Result<()>;
    /// Atomically move a file, symlink, or complete directory tree from `from`
    /// to `to`. File destinations may be replaced; callers must require an
    /// unoccupied destination before moving a directory.
    ///
    /// # Errors
    /// Returns an error when either path is invalid or the move cannot be completed.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Remove a file.
    ///
    /// # Errors
    /// Returns an error when the path is invalid, absent, or not a file.
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// Remove an empty directory.
    ///
    /// # Errors
    /// Returns an error when the directory is absent, non-empty, or not a directory.
    fn remove_dir(&self, path: &Path) -> Result<()>;
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
