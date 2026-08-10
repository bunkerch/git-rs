use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

use super::{FileSystem, MemoryFileSystem, Metadata, path::validate};

/// An in-memory writable layer over a read-only backing filesystem.
///
/// This is intentionally private: it provides the narrow copy-on-write view
/// used while validating quarantined packs. Existing files remain visible
/// through `lower`, while every mutation is isolated in `upper`.
#[derive(Clone)]
pub(crate) struct OverlayFileSystem {
    upper: MemoryFileSystem,
    lower: Arc<dyn FileSystem>,
}

impl OverlayFileSystem {
    pub(crate) fn new(lower: Arc<dyn FileSystem>) -> Self {
        Self {
            upper: MemoryFileSystem::new(),
            lower,
        }
    }

    fn prepare_parent(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or(Path::new(""));
        if !self.lower.metadata(parent).is_ok_and(Metadata::is_dir)
            && !self.upper.metadata(parent).is_ok_and(Metadata::is_dir)
        {
            return Err(Error::NotDirectory(parent.to_path_buf()));
        }
        self.upper.create_dir_all(parent)
    }

    fn upper_exists(&self, path: &Path) -> Result<bool> {
        self.upper.exists(path)
    }
}

impl FileSystem for OverlayFileSystem {
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let path = validate(path)?;
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            if self
                .metadata(&current)
                .is_ok_and(|metadata| !metadata.is_dir())
            {
                return Err(Error::NotDirectory(current));
            }
        }
        self.upper.create_dir_all(&path)
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let path = validate(path)?;
        match self.upper.read(&path) {
            Err(Error::NotFound(_)) => self.lower.read(&path),
            result => result,
        }
    }

    fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let path = validate(path)?;
        self.prepare_parent(&path)?;
        self.upper.write(&path, contents)
    }

    fn write_new(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let path = validate(path)?;
        if self.exists(&path)? {
            return Err(Error::AlreadyExists(path));
        }
        self.prepare_parent(&path)?;
        self.upper.write_new(&path, contents)
    }

    fn publish(&self, from: &Path, to: &Path) -> Result<()> {
        let from = validate(from)?;
        let to = validate(to)?;
        self.prepare_parent(&to)?;
        self.upper.publish(&from, &to)
    }

    fn read_link(&self, path: &Path) -> Result<Vec<u8>> {
        let path = validate(path)?;
        match self.upper.read_link(&path) {
            Err(Error::NotFound(_)) => self.lower.read_link(&path),
            result => result,
        }
    }

    fn create_symlink(&self, path: &Path, target: &[u8]) -> Result<()> {
        let path = validate(path)?;
        self.prepare_parent(&path)?;
        self.upper.create_symlink(&path, target)
    }

    fn set_executable(&self, path: &Path, executable: bool) -> Result<()> {
        let path = validate(path)?;
        if !self.upper_exists(&path)? {
            let contents = self.lower.read(&path)?;
            self.prepare_parent(&path)?;
            self.upper.write(&path, &contents)?;
        }
        self.upper.set_executable(&path, executable)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from = validate(from)?;
        let to = validate(to)?;
        if !self.upper_exists(&from)? {
            return Err(Error::NotFound(from));
        }
        self.prepare_parent(&to)?;
        self.upper.rename(&from, &to)
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        let path = validate(path)?;
        self.upper.remove_file(&path)
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        let path = validate(path)?;
        self.upper.remove_dir(&path)
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let path = validate(path)?;
        match self.upper.metadata(&path) {
            Err(Error::NotFound(_)) => self.lower.metadata(&path),
            result => result,
        }
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let path = validate(path)?;
        if !self.metadata(&path)?.is_dir() {
            return Err(Error::NotDirectory(path));
        }
        let mut children = BTreeSet::new();
        match self.lower.read_dir(&path) {
            Ok(entries) => children.extend(entries),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match self.upper.read_dir(&path) {
            Ok(entries) => children.extend(entries),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(children.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lower_and_isolates_writes_in_upper() {
        let lower = MemoryFileSystem::new();
        lower.create_dir_all(Path::new("objects/pack")).unwrap();
        lower.write(Path::new("HEAD"), b"lower").unwrap();
        lower
            .write(Path::new("objects/pack/base.idx"), b"base")
            .unwrap();
        let overlay = OverlayFileSystem::new(Arc::new(lower.clone()));

        assert_eq!(overlay.read(Path::new("HEAD")).unwrap(), b"lower");
        overlay.write(Path::new("HEAD"), b"upper").unwrap();
        overlay
            .write(Path::new("objects/pack/new.idx"), b"new")
            .unwrap();

        assert_eq!(overlay.read(Path::new("HEAD")).unwrap(), b"upper");
        assert_eq!(lower.read(Path::new("HEAD")).unwrap(), b"lower");
        assert_eq!(
            overlay.read_dir(Path::new("objects/pack")).unwrap(),
            vec![PathBuf::from("base.idx"), PathBuf::from("new.idx")]
        );
        assert!(!lower.exists(Path::new("objects/pack/new.idx")).unwrap());
    }

    #[test]
    fn create_only_write_observes_lower_layer() {
        let lower = MemoryFileSystem::new();
        lower.write(Path::new("HEAD"), b"lower").unwrap();
        let overlay = OverlayFileSystem::new(Arc::new(lower));

        assert!(matches!(
            overlay.write_new(Path::new("HEAD"), b"upper"),
            Err(Error::AlreadyExists(_))
        ));
    }
}
