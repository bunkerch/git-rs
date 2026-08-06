use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

use super::{FileStat, FileSystem, Metadata, path::validate};

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

    fn read_link(&self, path: &Path) -> Result<Vec<u8>> {
        let target = fs::read_link(self.resolve(path)?).map_err(|error| map_io(error, path))?;
        os_path_bytes(&target)
    }

    fn create_symlink(&self, path: &Path, target: &[u8]) -> Result<()> {
        let destination = self.resolve(path)?;
        if fs::symlink_metadata(&destination).is_ok() {
            fs::remove_file(&destination).map_err(|error| map_io(error, path))?;
        }
        create_host_symlink(target, &destination).map_err(Error::Io)
    }

    fn set_executable(&self, path: &Path, executable: bool) -> Result<()> {
        set_host_executable(&self.resolve(path)?, executable).map_err(Error::Io)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(self.resolve(from)?, self.resolve(to)?).map_err(|error| map_io(error, from))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(self.resolve(path)?).map_err(|error| map_io(error, path))
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        fs::remove_dir(self.resolve(path)?).map_err(|error| map_io(error, path))
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let metadata =
            fs::symlink_metadata(self.resolve(path)?).map_err(|error| map_io(error, path))?;
        let stat = host_stat(&metadata);
        if metadata.is_file() {
            Ok(Metadata::file(metadata.len())
                .with_executable(host_executable(&metadata))
                .with_stat(stat))
        } else if metadata.is_dir() {
            Ok(Metadata::directory().with_stat(stat))
        } else if metadata.file_type().is_symlink() {
            Ok(Metadata::symlink(metadata.len()).with_stat(stat))
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn os_path_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn os_path_bytes(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

#[cfg(unix)]
fn create_host_symlink(target: &[u8], destination: &Path) -> std::io::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(OsStr::from_bytes(target), destination)
}

#[cfg(windows)]
fn create_host_symlink(target: &[u8], destination: &Path) -> std::io::Result<()> {
    let target = std::str::from_utf8(target)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 symlink"))?;
    std::os::windows::fs::symlink_file(target, destination)
}

#[cfg(not(any(unix, windows)))]
fn create_host_symlink(_target: &[u8], _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symbolic links are unsupported",
    ))
}

#[cfg(unix)]
fn host_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn host_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_host_executable(path: &Path, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if executable {
        mode | 0o111
    } else {
        mode & !0o111
    });
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_host_executable(path: &Path, _executable: bool) -> std::io::Result<()> {
    if fs::metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a file",
        ))
    }
}

#[cfg(unix)]
fn host_stat(metadata: &fs::Metadata) -> FileStat {
    use std::os::unix::fs::MetadataExt;
    FileStat {
        ctime_seconds: metadata.ctime().try_into().unwrap_or(0),
        ctime_nanoseconds: metadata.ctime_nsec().try_into().unwrap_or(0),
        mtime_seconds: metadata.mtime().try_into().unwrap_or(0),
        mtime_nanoseconds: metadata.mtime_nsec().try_into().unwrap_or(0),
        device: u32::try_from(metadata.dev() & u64::from(u32::MAX))
            .expect("device was masked to u32"),
        inode: u32::try_from(metadata.ino() & u64::from(u32::MAX))
            .expect("inode was masked to u32"),
        uid: metadata.uid(),
        gid: metadata.gid(),
    }
}

#[cfg(not(unix))]
fn host_stat(_metadata: &fs::Metadata) -> FileStat {
    FileStat::default()
}

fn map_io(error: std::io::Error, path: &Path) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound(path.to_path_buf()),
        std::io::ErrorKind::AlreadyExists => Error::AlreadyExists(path.to_path_buf()),
        std::io::ErrorKind::IsADirectory => Error::IsDirectory(path.to_path_buf()),
        std::io::ErrorKind::NotADirectory => Error::NotDirectory(path.to_path_buf()),
        std::io::ErrorKind::DirectoryNotEmpty => Error::DirectoryNotEmpty(path.to_path_buf()),
        _ => Error::Io(error),
    }
}
