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
    common_dir: PathBuf,
    work_tree: Option<PathBuf>,
    pub(crate) pack_indexes: Arc<RwLock<BTreeMap<PathBuf, Arc<crate::PackIndex>>>>,
    pub(crate) pack_data: Arc<RwLock<BTreeMap<PathBuf, Arc<Vec<u8>>>>>,
    pub(crate) multi_pack_index: Arc<RwLock<Option<Arc<crate::MultiPackIndex>>>>,
    pub(crate) replacements: Arc<RwLock<Option<BTreeMap<crate::ObjectId, crate::ObjectId>>>>,
}

impl Repository {
    pub(crate) fn from_linked_parts(
        fs: Arc<dyn FileSystem>,
        git_dir: PathBuf,
        common_dir: PathBuf,
        work_tree: PathBuf,
    ) -> Self {
        Self {
            fs,
            git_dir,
            common_dir,
            work_tree: Some(work_tree),
            pack_indexes: Arc::new(RwLock::new(BTreeMap::new())),
            pack_data: Arc::new(RwLock::new(BTreeMap::new())),
            multi_pack_index: Arc::new(RwLock::new(None)),
            replacements: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) fn shared_filesystem(&self) -> Arc<dyn FileSystem> {
        Arc::clone(&self.fs)
    }

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
        let (git_dir, common_dir, work_tree) = if fs.exists(&non_bare.join("HEAD"))? {
            (non_bare.clone(), non_bare, Some(path.to_path_buf()))
        } else if fs.metadata(&non_bare).is_ok_and(crate::Metadata::is_file) {
            let pointer = parse_gitdir_file(&fs.read(&non_bare)?)?;
            let git_dir = resolve_indirection(path, &pointer)?;
            if !fs.exists(&git_dir.join("HEAD"))? {
                return Err(crate::Error::InvalidRepository(format!(
                    "{} points to a Git directory without HEAD",
                    non_bare.display()
                )));
            }
            let common_dir = match fs.read(&git_dir.join("commondir")) {
                Ok(contents) => resolve_indirection(&git_dir, &parse_path_line(&contents)?)?,
                Err(crate::Error::NotFound(_)) => git_dir.clone(),
                Err(error) => return Err(error),
            };
            (git_dir, common_dir, Some(path.to_path_buf()))
        } else if fs.exists(&path.join("HEAD"))? {
            (path.to_path_buf(), path.to_path_buf(), None)
        } else {
            return Err(crate::Error::InvalidRepository(format!(
                "{} has no Git HEAD",
                path.display()
            )));
        };
        let repository = Self {
            fs,
            git_dir,
            common_dir,
            work_tree,
            pack_indexes: Arc::new(RwLock::new(BTreeMap::new())),
            pack_data: Arc::new(RwLock::new(BTreeMap::new())),
            multi_pack_index: Arc::new(RwLock::new(None)),
            replacements: Arc::new(RwLock::new(None)),
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
            common_dir: git_dir.clone(),
            git_dir,
            work_tree,
            pack_indexes: Arc::new(RwLock::new(BTreeMap::new())),
            pack_data: Arc::new(RwLock::new(BTreeMap::new())),
            multi_pack_index: Arc::new(RwLock::new(None)),
            replacements: Arc::new(RwLock::new(None)),
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

    /// Directory containing objects, refs, configuration, and other state
    /// shared by every linked worktree.
    #[must_use]
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
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
        let path = path.as_ref();
        if self.git_dir != self.common_dir && is_common_path(path) {
            self.common_dir.join(path)
        } else {
            self.git_dir.join(path)
        }
    }

    /// Read a file relative to the repository's Git directory.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read from storage.
    pub fn read_git_file(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        self.fs.read(&self.git_path(path))
    }

    /// Publish a complete file using Git's lock-file-and-rename discipline.
    ///
    /// # Errors
    /// Returns an error if the temporary file cannot be written or published.
    pub fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let destination = self.git_path(path);
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

fn is_common_path(path: &Path) -> bool {
    let Some(first) = path.components().next() else {
        return false;
    };
    match first.as_os_str().to_str() {
        Some(
            "objects" | "refs" | "packed-refs" | "config" | "config.worktree" | "hooks" | "info"
            | "branches",
        ) => true,
        Some("logs") => path != Path::new("logs/HEAD"),
        _ => false,
    }
}

fn parse_gitdir_file(contents: &[u8]) -> Result<PathBuf> {
    let line = contents
        .strip_prefix(b"gitdir: ")
        .ok_or_else(|| crate::Error::InvalidRepository("invalid .git indirection".into()))?;
    parse_path_line(line)
}

fn parse_path_line(contents: &[u8]) -> Result<PathBuf> {
    let contents = contents.strip_suffix(b"\n").unwrap_or(contents);
    if contents.is_empty() || contents.contains(&0) || contents.contains(&b'\n') {
        return Err(crate::Error::InvalidRepository(
            "invalid Git directory path".into(),
        ));
    }
    let value = std::str::from_utf8(contents)
        .map_err(|_| crate::Error::InvalidRepository("non-UTF-8 Git directory path".into()))?;
    Ok(PathBuf::from(value))
}

fn resolve_indirection(base: &Path, value: &Path) -> Result<PathBuf> {
    use std::path::Component;

    if value.is_absolute() {
        return Err(crate::Error::InvalidRepository(
            "absolute Git directory paths are outside abstract storage".into(),
        ));
    }
    let mut components = base
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in value.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_os_string()),
            Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    crate::Error::InvalidRepository("Git directory path escapes storage".into())
                })?;
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(crate::Error::InvalidRepository(
                    "invalid Git directory path".into(),
                ));
            }
        }
    }
    Ok(components.into_iter().collect())
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
