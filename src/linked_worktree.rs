//! Git-compatible linked worktrees over abstract storage.

use std::path::{Component, Path, PathBuf};

use crate::{CheckoutOptions, Error, ObjectId, ReferenceName, Repository, Result, StatusOptions};

/// HEAD selection for a new linked worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeTarget {
    Branch(String),
    Detached(ObjectId),
}

/// Materialization and resource settings for worktree creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddWorktreeOptions {
    pub checkout: bool,
    pub max_object_size: usize,
}

impl Default for AddWorktreeOptions {
    fn default() -> Self {
        Self {
            checkout: true,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// One registered linked worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkedWorktreeInfo {
    pub name: String,
    pub path: PathBuf,
    pub target: WorktreeTarget,
}

impl Repository {
    /// Create and register a linked worktree sharing this repository's object
    /// database, refs, and configuration.
    ///
    /// `name` is the stable administrative identifier below
    /// `worktrees/<name>`. Branch targets may be checked out by only one
    /// worktree.
    ///
    /// # Errors
    /// Returns an error for unsafe or occupied paths/names, a missing or
    /// already-checked-out branch, a non-commit target, checkout conflicts,
    /// or storage failures.
    pub fn add_worktree(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        target: &WorktreeTarget,
        options: &AddWorktreeOptions,
    ) -> Result<Self> {
        validate_worktree_name(name)?;
        let path = normalized_storage_path(path.as_ref())?;
        if path.as_os_str().is_empty() || self.filesystem().exists(&path)? {
            return Err(Error::AlreadyExists(path));
        }
        let admin = self.common_dir().join("worktrees").join(name);
        if self.filesystem().exists(&admin)? {
            return Err(Error::AlreadyExists(admin));
        }

        let (head, commit_id) = match target {
            WorktreeTarget::Branch(branch) => {
                let reference = ReferenceName::branch(branch)?;
                self.ensure_branch_available(&reference)?;
                let id = self.resolve_reference(reference.as_str())?;
                self.read_commit(id, options.max_object_size)?;
                (format!("ref: {}\n", reference.as_str()), id)
            }
            WorktreeTarget::Detached(id) => {
                self.read_commit(*id, options.max_object_size)?;
                (format!("{id}\n"), *id)
            }
        };

        self.enable_relative_worktrees()?;
        self.filesystem().create_dir_all(&admin)?;
        self.filesystem().create_dir_all(&path)?;
        let dot_git = path.join(".git");
        let result = (|| {
            self.filesystem().write(
                &dot_git,
                format!("gitdir: {}\n", relative_path(&path, &admin)?.display()).as_bytes(),
            )?;
            self.filesystem().write(
                &admin.join("gitdir"),
                format!("{}\n", relative_path(&admin, &dot_git)?.display()).as_bytes(),
            )?;
            self.filesystem().write(
                &admin.join("commondir"),
                format!("{}\n", relative_path(&admin, self.common_dir())?.display()).as_bytes(),
            )?;
            self.filesystem()
                .write(&admin.join("HEAD"), head.as_bytes())?;

            let linked = Repository::from_linked_parts(
                self.shared_filesystem(),
                admin.clone(),
                self.common_dir().to_path_buf(),
                path.clone(),
            );
            if options.checkout {
                let commit = linked.read_commit(commit_id, options.max_object_size)?;
                linked.checkout_tree(
                    commit.tree(),
                    &CheckoutOptions {
                        force: true,
                        max_object_size: options.max_object_size,
                    },
                )?;
            }
            Ok(linked)
        })();
        if result.is_err() {
            let _ = remove_tree(self.filesystem(), &path);
            let _ = remove_tree(self.filesystem(), &admin);
        }
        result
    }

    /// List registered linked worktrees in administrative-name order.
    ///
    /// # Errors
    /// Returns an error for corrupt registration files or storage failures.
    pub fn linked_worktrees(&self) -> Result<Vec<LinkedWorktreeInfo>> {
        let root = self.common_dir().join("worktrees");
        let names = match self.filesystem().read_dir(&root) {
            Ok(names) => names,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut result = Vec::with_capacity(names.len());
        for entry in names {
            let name = entry
                .to_str()
                .ok_or_else(|| Error::InvalidRepository("non-UTF-8 worktree name".into()))?
                .to_owned();
            validate_worktree_name(&name)?;
            let admin = root.join(&entry);
            let backlink = parse_path_file(&self.filesystem().read(&admin.join("gitdir"))?)?;
            let dot_git = resolve_relative(&admin, &backlink)?;
            let path = dot_git
                .parent()
                .ok_or_else(|| Error::InvalidRepository("invalid worktree backlink".into()))?
                .to_path_buf();
            let target = parse_head(&self.filesystem().read(&admin.join("HEAD"))?)?;
            result.push(LinkedWorktreeInfo { name, path, target });
        }
        Ok(result)
    }

    /// Remove a linked worktree and its administrative directory.
    ///
    /// Without `force`, staged, unstaged, or untracked changes prevent
    /// removal. The main worktree cannot be removed through this API.
    ///
    /// # Errors
    /// Returns an error for an invalid or missing registration, dirty worktree,
    /// corrupt backlink, or storage failure.
    pub fn remove_worktree(&self, name: &str, force: bool) -> Result<()> {
        validate_worktree_name(name)?;
        let admin = self.common_dir().join("worktrees").join(name);
        let backlink = parse_path_file(&self.filesystem().read(&admin.join("gitdir"))?)?;
        let dot_git = resolve_relative(&admin, &backlink)?;
        let path = dot_git
            .parent()
            .ok_or_else(|| Error::InvalidRepository("invalid worktree backlink".into()))?
            .to_path_buf();
        if self.filesystem().exists(&path)? {
            if !force {
                let linked = Repository::open_shared(self.shared_filesystem(), &path)?;
                let status = linked.status(&StatusOptions::default())?;
                if !status.is_clean() {
                    return Err(Error::CheckoutConflict(
                        status
                            .entries()
                            .iter()
                            .map(|entry| String::from_utf8_lossy(entry.path()).into_owned())
                            .collect(),
                    ));
                }
            }
            remove_tree(self.filesystem(), &path)?;
        }
        remove_tree(self.filesystem(), &admin)
    }

    fn ensure_branch_available(&self, branch: &ReferenceName) -> Result<()> {
        if symbolic_head_matches(
            &self.filesystem().read(&self.common_dir().join("HEAD"))?,
            branch,
        ) {
            return Err(Error::ReferenceConflict(format!(
                "{branch} is checked out in the main worktree"
            )));
        }
        for worktree in self.linked_worktrees()? {
            if worktree.target
                == WorktreeTarget::Branch(
                    branch
                        .as_str()
                        .strip_prefix("refs/heads/")
                        .expect("branch reference prefix")
                        .to_owned(),
                )
            {
                return Err(Error::ReferenceConflict(format!(
                    "{} is checked out in worktree {}",
                    branch, worktree.name
                )));
            }
        }
        Ok(())
    }

    fn enable_relative_worktrees(&self) -> Result<()> {
        let bytes = self.filesystem().read(&self.common_dir().join("config"))?;
        let mut config = String::from_utf8(bytes)
            .map_err(|_| Error::InvalidRepository("config is not UTF-8".into()))?;
        if config.contains("relativeWorktrees = true")
            || config.contains("relativeworktrees = true")
        {
            return Ok(());
        }
        let version = "\trepositoryformatversion = 0\n";
        if !config.contains(version) {
            return Err(Error::InvalidRepository(
                "cannot enable relative worktrees for unknown repository format".into(),
            ));
        }
        config = config.replacen(version, "\trepositoryformatversion = 1\n", 1);
        config.push_str("[extensions]\n\trelativeWorktrees = true\n");
        self.write_atomic(Path::new("config"), config.as_bytes())
    }
}

fn parse_head(contents: &[u8]) -> Result<WorktreeTarget> {
    let line = contents.strip_suffix(b"\n").unwrap_or(contents);
    if let Some(name) = line.strip_prefix(b"ref: ") {
        let name = std::str::from_utf8(name)
            .map_err(|_| Error::InvalidReference("non-UTF-8 worktree HEAD".into()))?;
        let branch = name.strip_prefix("refs/heads/").ok_or_else(|| {
            Error::InvalidReference("worktree HEAD does not name a branch".into())
        })?;
        ReferenceName::branch(branch)?;
        Ok(WorktreeTarget::Branch(branch.to_owned()))
    } else {
        let id = std::str::from_utf8(line)
            .map_err(|_| Error::InvalidReference("non-ASCII worktree HEAD".into()))?
            .parse()
            .map_err(|_| Error::InvalidReference("invalid detached worktree HEAD".into()))?;
        Ok(WorktreeTarget::Detached(id))
    }
}

fn symbolic_head_matches(contents: &[u8], branch: &ReferenceName) -> bool {
    contents == format!("ref: {}\n", branch.as_str()).as_bytes()
}

fn validate_worktree_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::InvalidRepository(format!(
            "invalid worktree name `{name}`"
        )));
    }
    Ok(())
}

fn normalized_storage_path(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => result.push(value),
            _ => return Err(Error::InvalidPath(path.to_path_buf())),
        }
    }
    Ok(result)
}

fn relative_path(from: &Path, to: &Path) -> Result<PathBuf> {
    let from = normal_components(from)?;
    let to = normal_components(to)?;
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in shared..from.len() {
        result.push("..");
    }
    result.extend(to[shared..].iter());
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Ok(result)
}

fn normal_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            _ => Err(Error::InvalidPath(path.to_path_buf())),
        })
        .collect()
}

fn parse_path_file(contents: &[u8]) -> Result<PathBuf> {
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("non-UTF-8 worktree path".into()))?;
    if value.is_empty() || value.contains(['\0', '\n', '\r']) {
        return Err(Error::InvalidRepository("invalid worktree path".into()));
    }
    Ok(PathBuf::from(value))
}

fn resolve_relative(base: &Path, value: &Path) -> Result<PathBuf> {
    let mut components = normal_components(base)?;
    for component in value.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_os_string()),
            Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    Error::InvalidRepository("worktree path escapes storage".into())
                })?;
            }
            _ => return Err(Error::InvalidRepository("invalid worktree path".into())),
        }
    }
    Ok(components.into_iter().collect())
}

fn remove_tree(filesystem: &dyn crate::FileSystem, path: &Path) -> Result<()> {
    let metadata = filesystem.metadata(path)?;
    if metadata.is_dir() {
        for child in filesystem.read_dir(path)? {
            remove_tree(filesystem, &path.join(child))?;
        }
        filesystem.remove_dir(path)
    } else {
        filesystem.remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{AddWorktreeOptions, WorktreeTarget};
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, ObjectKind,
        PreviousValue, ReferenceName, Repository, Signature, Tree, TreeEntry,
    };

    #[test]
    fn creates_opens_and_lists_git_compatible_linked_layout() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let main_tip = commit(&repository, None, b"main\n");
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                main_tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let topic_tip = commit(&repository, Some(main_tip), b"topic\n");
        repository.create_branch("topic", topic_tip, false).unwrap();

        let linked = repository
            .add_worktree(
                "topic-work",
                "topic-work",
                &WorktreeTarget::Branch("topic".to_owned()),
                &AddWorktreeOptions::default(),
            )
            .unwrap();
        assert_eq!(
            linked.git_dir(),
            Path::new("main/.git/worktrees/topic-work")
        );
        assert_eq!(linked.common_dir(), Path::new("main/.git"));
        assert_eq!(linked.resolve_reference("HEAD").unwrap(), topic_tip);
        assert_eq!(
            filesystem.read(Path::new("topic-work/file")).unwrap(),
            b"topic\n"
        );
        assert!(filesystem.exists(Path::new("topic-work/.git")).unwrap());

        let reopened = Repository::open(filesystem.clone(), "topic-work").unwrap();
        assert_eq!(
            reopened.resolve_reference("refs/heads/main").unwrap(),
            main_tip
        );
        assert_eq!(reopened.resolve_reference("HEAD").unwrap(), topic_tip);
        assert_ne!(reopened.git_path("index"), repository.git_path("index"));
        assert_eq!(reopened.git_path("objects"), repository.git_path("objects"));

        let listed = repository.linked_worktrees().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, Path::new("topic-work"));
        assert_eq!(listed[0].target, WorktreeTarget::Branch("topic".to_owned()));
        assert!(
            repository
                .add_worktree(
                    "duplicate",
                    "duplicate",
                    &WorktreeTarget::Branch("topic".to_owned()),
                    &AddWorktreeOptions::default(),
                )
                .is_err()
        );
        assert!(
            repository
                .add_worktree(
                    "main-duplicate",
                    "main-duplicate",
                    &WorktreeTarget::Branch("main".to_owned()),
                    &AddWorktreeOptions::default(),
                )
                .is_err()
        );

        let detached = repository
            .add_worktree(
                "detached-work",
                "detached-work",
                &WorktreeTarget::Detached(main_tip),
                &AddWorktreeOptions::default(),
            )
            .unwrap();
        assert_eq!(
            detached.read_git_file("HEAD").unwrap(),
            format!("{main_tip}\n").as_bytes()
        );
        repository.remove_worktree("detached-work", false).unwrap();

        let config = String::from_utf8(repository.read_git_file("config").unwrap()).unwrap();
        assert!(config.contains("repositoryformatversion = 1"));
        assert!(config.contains("relativeWorktrees = true"));

        filesystem
            .write(Path::new("topic-work/file"), b"dirty\n")
            .unwrap();
        assert!(repository.remove_worktree("topic-work", false).is_err());
        repository.remove_worktree("topic-work", true).unwrap();
        assert!(!filesystem.exists(Path::new("topic-work")).unwrap());
        assert!(repository.linked_worktrees().unwrap().is_empty());
    }

    fn commit(
        repository: &Repository,
        parent: Option<crate::ObjectId>,
        contents: &[u8],
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("Worktree", "worktree@example.com", 1, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository
            .write_commit(&builder.message(b"worktree\n".to_vec()).build())
            .unwrap()
    }
}
