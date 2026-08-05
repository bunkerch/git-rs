use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

use super::{FileSystem, Metadata, path::validate};

#[derive(Clone, Debug)]
pub struct HostFileSystem {
    root: PathBuf,
}

impl HostFileSystem {
    /// Create a host adapter rooted at `root`, creating the root when needed.
    ///
    /// # Errors
    /// Returns an I/O error when the root directory cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf> {
        Ok(self.root.join(validate(path)?))
    }
}

impl FileSystem for HostFileSystem {
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(self.resolve(path)?)?;
        Ok(())
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(self.resolve(path)?).map_err(|error| map_io(error, path))
    }

    fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
        fs::write(self.resolve(path)?, contents).map_err(|error| map_io(error, path))
    }

    fn write_new(&self, path: &Path, contents: &[u8]) -> Result<()> {
        use std::io::Write;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.resolve(path)?)
            .map_err(|error| map_io(error, path))?;
        file.write_all(contents).map_err(Error::Io)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(self.resolve(from)?, self.resolve(to)?).map_err(|error| map_io(error, from))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(self.resolve(path)?).map_err(|error| map_io(error, path))
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let metadata = fs::metadata(self.resolve(path)?).map_err(|error| map_io(error, path))?;
        if metadata.is_file() {
            Ok(Metadata::file(metadata.len()))
        } else if metadata.is_dir() {
            Ok(Metadata::directory())
        } else {
            Err(Error::InvalidPath(path.to_path_buf()))
        }
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut entries = fs::read_dir(self.resolve(path)?)
            .map_err(|error| map_io(error, path))?
            .map(|entry| entry.map(|entry| PathBuf::from(entry.file_name())))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_unstable();
        Ok(entries)
    }
}

fn map_io(error: std::io::Error, path: &Path) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound(path.to_path_buf()),
        std::io::ErrorKind::AlreadyExists => Error::AlreadyExists(path.to_path_buf()),
        std::io::ErrorKind::IsADirectory => Error::IsDirectory(path.to_path_buf()),
        std::io::ErrorKind::NotADirectory => Error::NotDirectory(path.to_path_buf()),
        _ => Error::Io(error),
    }
}
