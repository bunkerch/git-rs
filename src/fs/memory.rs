use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::{Error, Result};

use super::{FileSystem, Metadata, path::validate};

#[derive(Clone, Debug)]
pub struct MemoryFileSystem {
    entries: Arc<RwLock<BTreeMap<PathBuf, Entry>>>,
}

#[derive(Clone, Debug)]
enum Entry {
    File { data: Vec<u8>, executable: bool },
    Directory,
    Symlink(Vec<u8>),
}

impl Default for MemoryFileSystem {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(PathBuf::new(), Entry::Directory);
        Self {
            entries: Arc::new(RwLock::new(entries)),
        }
    }
}

impl MemoryFileSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<PathBuf, Entry>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn entries_mut(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<PathBuf, Entry>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl FileSystem for MemoryFileSystem {
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let path = validate(path)?;
        let mut entries = self.entries_mut();
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            match entries.get(&current) {
                Some(Entry::File { .. } | Entry::Symlink(_)) => {
                    return Err(Error::NotDirectory(current));
                }
                Some(Entry::Directory) => {}
                None => {
                    entries.insert(current.clone(), Entry::Directory);
                }
            }
        }
        Ok(())
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let path = validate(path)?;
        match self.entries().get(&path) {
            Some(Entry::File { data, .. }) => Ok(data.clone()),
            Some(Entry::Directory) => Err(Error::IsDirectory(path)),
            Some(Entry::Symlink(_)) => Err(Error::InvalidPath(path)),
            None => Err(Error::NotFound(path)),
        }
    }

    fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let path = validate(path)?;
        let parent = path.parent().unwrap_or(Path::new(""));
        match self.entries().get(parent) {
            Some(Entry::Directory) => {}
            Some(Entry::File { .. } | Entry::Symlink(_)) => {
                return Err(Error::NotDirectory(parent.to_path_buf()));
            }
            None => return Err(Error::NotFound(parent.to_path_buf())),
        }
        if matches!(self.entries().get(&path), Some(Entry::Directory)) {
            return Err(Error::IsDirectory(path));
        }
        let executable = matches!(
            self.entries().get(&path),
            Some(Entry::File {
                executable: true,
                ..
            })
        );
        self.entries_mut().insert(
            path,
            Entry::File {
                data: contents.to_vec(),
                executable,
            },
        );
        Ok(())
    }

    fn write_new(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let path = validate(path)?;
        let parent = path.parent().unwrap_or(Path::new(""));
        let mut entries = self.entries_mut();
        match entries.get(parent) {
            Some(Entry::Directory) => {}
            Some(Entry::File { .. } | Entry::Symlink(_)) => {
                return Err(Error::NotDirectory(parent.to_path_buf()));
            }
            None => return Err(Error::NotFound(parent.to_path_buf())),
        }
        if entries.contains_key(&path) {
            return Err(Error::AlreadyExists(path));
        }
        entries.insert(
            path,
            Entry::File {
                data: contents.to_vec(),
                executable: false,
            },
        );
        Ok(())
    }

    fn read_link(&self, path: &Path) -> Result<Vec<u8>> {
        let path = validate(path)?;
        match self.entries().get(&path) {
            Some(Entry::Symlink(target)) => Ok(target.clone()),
            Some(_) => Err(Error::InvalidPath(path)),
            None => Err(Error::NotFound(path)),
        }
    }

    fn create_symlink(&self, path: &Path, target: &[u8]) -> Result<()> {
        let path = validate(path)?;
        let parent = path.parent().unwrap_or(Path::new(""));
        let mut entries = self.entries_mut();
        if !matches!(entries.get(parent), Some(Entry::Directory)) {
            return Err(Error::NotDirectory(parent.to_path_buf()));
        }
        if matches!(entries.get(&path), Some(Entry::Directory)) {
            return Err(Error::IsDirectory(path));
        }
        entries.insert(path, Entry::Symlink(target.to_vec()));
        Ok(())
    }

    fn set_executable(&self, path: &Path, executable: bool) -> Result<()> {
        let path = validate(path)?;
        let mut entries = self.entries_mut();
        match entries.get_mut(&path) {
            Some(Entry::File {
                executable: value, ..
            }) => {
                *value = executable;
                Ok(())
            }
            Some(_) => Err(Error::InvalidPath(path)),
            None => Err(Error::NotFound(path)),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from = validate(from)?;
        let to = validate(to)?;
        let mut entries = self.entries_mut();
        let entry = entries
            .remove(&from)
            .ok_or_else(|| Error::NotFound(from.clone()))?;
        if matches!(entry, Entry::Directory) {
            entries.insert(from.clone(), entry);
            return Err(Error::IsDirectory(from));
        }
        let parent = to.parent().unwrap_or(Path::new(""));
        if !matches!(entries.get(parent), Some(Entry::Directory)) {
            entries.insert(from, entry);
            return Err(Error::NotDirectory(parent.to_path_buf()));
        }
        entries.insert(to, entry);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        let path = validate(path)?;
        let mut entries = self.entries_mut();
        match entries.get(&path) {
            Some(Entry::File { .. } | Entry::Symlink(_)) => {
                entries.remove(&path);
                Ok(())
            }
            Some(Entry::Directory) => Err(Error::IsDirectory(path)),
            None => Err(Error::NotFound(path)),
        }
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let path = validate(path)?;
        match self.entries().get(&path) {
            Some(Entry::File { data, executable }) => {
                Ok(Metadata::file(data.len() as u64).with_executable(*executable))
            }
            Some(Entry::Directory) => Ok(Metadata::directory()),
            Some(Entry::Symlink(target)) => Ok(Metadata::symlink(target.len() as u64)),
            None => Err(Error::NotFound(path)),
        }
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let path = validate(path)?;
        match self.entries().get(&path) {
            Some(Entry::Directory) => {}
            Some(Entry::File { .. } | Entry::Symlink(_)) => {
                return Err(Error::NotDirectory(path));
            }
            None => return Err(Error::NotFound(path)),
        }
        let entries = self.entries();
        let mut children = entries
            .keys()
            .filter(|candidate| candidate.parent() == Some(path.as_path()))
            .filter_map(|candidate| candidate.file_name().map(PathBuf::from))
            .collect::<Vec<_>>();
        children.sort_unstable();
        Ok(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_storage() {
        let first = MemoryFileSystem::new();
        let second = first.clone();
        first.create_dir_all(Path::new("objects/pack")).unwrap();
        first
            .write(Path::new("objects/pack/a.pack"), b"pack")
            .unwrap();
        assert_eq!(
            second.read(Path::new("objects/pack/a.pack")).unwrap(),
            b"pack"
        );
    }

    #[test]
    fn rename_atomically_replaces_a_file() {
        let fs = MemoryFileSystem::new();
        fs.write(Path::new("HEAD"), b"old").unwrap();
        fs.write(Path::new("HEAD.lock"), b"new").unwrap();
        fs.rename(Path::new("HEAD.lock"), Path::new("HEAD"))
            .unwrap();
        assert_eq!(fs.read(Path::new("HEAD")).unwrap(), b"new");
        assert!(!fs.exists(Path::new("HEAD.lock")).unwrap());
    }

    #[test]
    fn create_only_write_acquires_a_lock_once() {
        let fs = MemoryFileSystem::new();
        fs.write_new(Path::new("HEAD.lock"), b"first").unwrap();
        assert!(matches!(
            fs.write_new(Path::new("HEAD.lock"), b"second"),
            Err(Error::AlreadyExists(_))
        ));
        assert_eq!(fs.read(Path::new("HEAD.lock")).unwrap(), b"first");
    }
}
