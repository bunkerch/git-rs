use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::{FileSystem, Result};

#[derive(Clone, Debug)]
pub struct InitOptions {
    pub bare: bool,
    pub initial_branch: String,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            bare: false,
            initial_branch: "main".to_owned(),
        }
    }
}

#[derive(Clone)]
pub struct Repository {
    fs: Arc<dyn FileSystem>,
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    pub(crate) pack_indexes: Arc<RwLock<BTreeMap<PathBuf, Arc<crate::PackIndex>>>>,
    pub(crate) pack_data: Arc<RwLock<BTreeMap<PathBuf, Arc<Vec<u8>>>>>,
}

impl Repository {
    /// Open an existing bare or non-bare repository.
    ///
    /// A path containing `.git/HEAD` is treated as a working tree. Otherwise,
    /// the path itself is treated as a bare Git directory.
    ///
    /// # Errors
    /// Returns an error when neither layout contains a valid `HEAD` file or a
    /// storage operation fails.
    pub fn open<F: FileSystem>(fs: F, path: impl AsRef<Path>) -> Result<Self> {
        Self::open_shared(Arc::new(fs), path)
    }

    /// Open an existing repository using shared, dynamically dispatched storage.
    ///
    /// # Errors
    /// Returns an error when the repository layout is invalid or storage fails.
    pub fn open_shared(fs: Arc<dyn FileSystem>, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let non_bare = path.join(".git");
        let (git_dir, work_tree) = if fs.exists(&non_bare.join("HEAD"))? {
            (non_bare, Some(path.to_path_buf()))
        } else if fs.exists(&path.join("HEAD"))? {
            (path.to_path_buf(), None)
        } else {
            return Err(crate::Error::InvalidRepository(format!(
                "{} has no Git HEAD",
                path.display()
            )));
        };
        let repository = Self {
            fs,
            git_dir,
            work_tree,
            pack_indexes: Arc::new(RwLock::new(BTreeMap::new())),
            pack_data: Arc::new(RwLock::new(BTreeMap::new())),
        };
        repository.read_reference("HEAD")?;
        Ok(repository)
    }

    /// Initialize a repository in `fs`.
    ///
    /// # Errors
    /// Returns an error for invalid options or any failed storage operation.
    pub fn init<F: FileSystem>(
        fs: F,
        path: impl AsRef<Path>,
        options: &InitOptions,
    ) -> Result<Self> {
        Self::init_shared(Arc::new(fs), path, options)
    }

    /// Initialize a repository using shared, dynamically dispatched storage.
    ///
    /// # Errors
    /// Returns an error for invalid options or any failed storage operation.
    pub fn init_shared(
        fs: Arc<dyn FileSystem>,
        path: impl AsRef<Path>,
        options: &InitOptions,
    ) -> Result<Self> {
        validate_branch_name(&options.initial_branch)?;
        let path = path.as_ref();
        let (git_dir, work_tree) = if options.bare {
            (path.to_path_buf(), None)
        } else {
            (path.join(".git"), Some(path.to_path_buf()))
        };

        for directory in [
            "branches",
            "hooks",
            "info",
            "objects/info",
            "objects/pack",
            "refs/heads",
            "refs/tags",
        ] {
            fs.create_dir_all(&git_dir.join(directory))?;
        }

        let repository = Self {
            fs,
            git_dir,
            work_tree,
            pack_indexes: Arc::new(RwLock::new(BTreeMap::new())),
            pack_data: Arc::new(RwLock::new(BTreeMap::new())),
        };
        repository.write_atomic(
            Path::new("HEAD"),
            format!("ref: refs/heads/{}\n", options.initial_branch).as_bytes(),
        )?;
        repository.write_atomic(Path::new("config"), config(options.bare).as_bytes())?;
        repository.write_atomic(
            Path::new("description"),
            b"Unnamed repository; edit this file 'description' to name the repository.\n",
        )?;
        Ok(repository)
    }

    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    #[must_use]
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    #[must_use]
    pub fn filesystem(&self) -> &dyn FileSystem {
        self.fs.as_ref()
    }

    #[must_use]
    pub(crate) fn git_path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.git_dir.join(path)
    }

    /// Read a file relative to the repository's Git directory.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read from storage.
    pub fn read_git_file(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        self.fs.read(&self.git_dir.join(path))
    }

    /// Publish a complete file using Git's lock-file-and-rename discipline.
    ///
    /// # Errors
    /// Returns an error if the temporary file cannot be written or published.
    pub fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let destination = self.git_dir.join(path);
        if let Some(parent) = destination.parent() {
            self.fs.create_dir_all(parent)?;
        }
        let lock = destination.with_extension("lock");
        self.fs.write_new(&lock, contents)?;
        if let Err(error) = self.fs.rename(&lock, &destination) {
            let _ = self.fs.remove_file(&lock);
            return Err(error);
        }
        Ok(())
    }
}

fn config(bare: bool) -> String {
    format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = {}\n\tlogallrefupdates = {}\n",
        bare, !bare,
    )
}

// Git's ref grammar specifies the lowercase byte suffix exactly.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn validate_branch_name(name: &str) -> Result<()> {
    if crate::refs::ReferenceName::branch(name).is_err() {
        return Err(crate::Error::InvalidRepository(format!(
            "invalid initial branch name `{name}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryFileSystem;

    #[test]
    fn initializes_git_compatible_non_bare_layout_in_memory() {
        let fs = MemoryFileSystem::new();
        let repository =
            Repository::init(fs.clone(), Path::new("project"), &InitOptions::default()).unwrap();
        assert_eq!(repository.git_dir(), Path::new("project/.git"));
        assert_eq!(repository.work_tree(), Some(Path::new("project")));
        assert_eq!(
            repository.read_git_file("HEAD").unwrap(),
            b"ref: refs/heads/main\n"
        );
        assert!(
            fs.metadata(Path::new("project/.git/objects/pack"))
                .unwrap()
                .is_dir()
        );
        assert!(
            String::from_utf8(repository.read_git_file("config").unwrap())
                .unwrap()
                .contains("\tbare = false\n")
        );
    }

    #[test]
    fn initializes_bare_layout_without_dot_git() {
        let fs = MemoryFileSystem::new();
        let options = InitOptions {
            bare: true,
            initial_branch: "trunk".to_owned(),
        };
        let repository = Repository::init(fs.clone(), Path::new("server.git"), &options).unwrap();
        assert_eq!(repository.git_dir(), Path::new("server.git"));
        assert_eq!(repository.work_tree(), None);
        assert_eq!(
            fs.read(Path::new("server.git/HEAD")).unwrap(),
            b"ref: refs/heads/trunk\n"
        );
    }

    #[test]
    fn rejects_invalid_initial_branch_before_writing() {
        let fs = MemoryFileSystem::new();
        let options = InitOptions {
            initial_branch: "bad..name".to_owned(),
            ..InitOptions::default()
        };
        assert!(Repository::init(fs.clone(), Path::new("project"), &options).is_err());
        assert!(!fs.exists(Path::new("project")).unwrap());
    }

    #[test]
    fn reopens_bare_and_non_bare_repositories() {
        let fs = MemoryFileSystem::new();
        Repository::init(fs.clone(), "work", &InitOptions::default()).unwrap();
        Repository::init(
            fs.clone(),
            "bare.git",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();

        assert_eq!(
            Repository::open(fs.clone(), "work").unwrap().work_tree(),
            Some(Path::new("work"))
        );
        assert_eq!(Repository::open(fs, "bare.git").unwrap().work_tree(), None);
    }
}
