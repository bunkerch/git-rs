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

/// Safety settings for linked-worktree removal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RemoveWorktreeOptions {
    /// Permit removal when the linked worktree has local changes.
    pub force: bool,
    /// Permit removal of a locked worktree. Git spells this second authority
    /// as a second `--force`; it is kept distinct in the library API.
    pub override_lock: bool,
}

/// Collision and lock authority for linked-worktree moves.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MoveWorktreeOptions {
    /// Replace a missing, already-registered destination.
    pub force_registered_destination: bool,
    /// Move a locked source or replace a locked missing destination.
    pub override_locks: bool,
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

/// Why a linked-worktree administrative entry is eligible for pruning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreePruneReason {
    NotDirectory,
    MissingGitdir,
    InvalidGitdir,
    MissingWorktree,
    Duplicate,
}

/// One administrative entry selected by [`Repository::prune_worktrees`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreePruneEntry {
    name: String,
    path: PathBuf,
    reason: WorktreePruneReason,
}

impl WorktreePruneEntry {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub const fn reason(&self) -> WorktreePruneReason {
        self.reason
    }
}

/// Resource and mutation settings for linked-worktree pruning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreePruneOptions {
    /// Missing worktrees whose administrative index is no newer than this are
    /// eligible. `u64::MAX` matches Git's default of pruning all missing ones.
    pub expire_before: u64,
    pub max_worktrees: usize,
    pub dry_run: bool,
}

impl Default for WorktreePruneOptions {
    fn default() -> Self {
        Self {
            expire_before: u64::MAX,
            max_worktrees: 100_000,
            dry_run: true,
        }
    }
}

impl Repository {
    /// Move a linked worktree directory and update both Git linking files.
    ///
    /// When `destination` is an existing directory, the source directory name
    /// is appended, matching `git worktree move`. The returned path is the
    /// resolved destination in the adapter namespace.
    ///
    /// # Errors
    /// Returns an error for invalid registration links, occupied or registered
    /// destinations without authority, locks, populated submodules, adapter
    /// rename failure, or link-repair failure.
    pub fn move_worktree(
        &self,
        name: &str,
        destination: impl AsRef<Path>,
        options: &MoveWorktreeOptions,
    ) -> Result<PathBuf> {
        let admin = self.worktree_admin_dir(name)?;
        if self.worktree_lock_reason(name)?.is_some() && !options.override_locks {
            return Err(Error::ReferenceConflict(format!(
                "worktree `{name}` is locked"
            )));
        }
        let backlink = parse_path_file(&self.filesystem().read(&admin.join("gitdir"))?)?;
        let dot_git = resolve_worktree_path(&admin, &backlink)?;
        let source = dot_git
            .parent()
            .ok_or_else(|| Error::InvalidRepository("invalid worktree backlink".into()))?
            .to_path_buf();
        if !self.filesystem().metadata(&source)?.is_dir() {
            return Err(Error::NotDirectory(source));
        }
        self.validate_worktree_gitfile(&source, &admin)?;
        self.reject_movable_worktree_submodules(&admin, &source)?;

        let mut destination = normalized_storage_path(destination.as_ref())?;
        match self.filesystem().metadata(&destination) {
            Ok(metadata) if metadata.is_dir() => {
                let basename = source.file_name().ok_or_else(|| {
                    Error::InvalidRepository("worktree has no directory name".into())
                })?;
                destination.push(basename);
            }
            Ok(_) => return Err(Error::AlreadyExists(destination)),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        if destination == source || destination.starts_with(&source) {
            return Err(Error::InvalidPath(destination));
        }
        if self.filesystem().exists(&destination)? {
            return Err(Error::AlreadyExists(destination));
        }
        self.remove_registered_move_destination(name, &destination, options)?;

        self.filesystem().rename(&source, &destination)?;
        if let Err(error) = self.repair_worktree(name, &destination) {
            let _ = self.filesystem().rename(&destination, &source);
            return Err(error);
        }
        Ok(destination)
    }

    /// Repair both linking files after a linked worktree has been moved to
    /// `path` outside this library.
    ///
    /// Returns `true` when either file changed. Paths are written in Git's
    /// relative-worktree format so the result remains portable inside an
    /// abstract storage namespace.
    ///
    /// # Errors
    /// Returns an error for an invalid registration or destination, a
    /// non-directory destination, a directory at `<path>/.git`, lock
    /// contention, or storage publication failure.
    pub fn repair_worktree(&self, name: &str, path: impl AsRef<Path>) -> Result<bool> {
        let admin = self.worktree_admin_dir(name)?;
        let path = normalized_storage_path(path.as_ref())?;
        let metadata = self.filesystem().metadata(&path)?;
        if !metadata.is_dir() {
            return Err(Error::NotDirectory(path));
        }
        let dot_git = path.join(".git");
        match self.filesystem().metadata(&dot_git) {
            Ok(metadata) if metadata.is_dir() => return Err(Error::IsDirectory(dot_git)),
            Ok(_) | Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let dot_git_contents =
            format!("gitdir: {}\n", relative_path(&path, &admin)?.display()).into_bytes();
        let admin_contents =
            format!("{}\n", relative_path(&admin, &dot_git)?.display()).into_bytes();
        let current_dot_git = optional_read(self.filesystem(), &dot_git)?;
        let current_admin = optional_read(self.filesystem(), &admin.join("gitdir"))?;
        if current_dot_git.as_deref() == Some(dot_git_contents.as_slice())
            && current_admin.as_deref() == Some(admin_contents.as_slice())
        {
            return Ok(false);
        }

        self.enable_relative_worktrees()?;
        publish_linking_files(
            self.filesystem(),
            &admin.join("gitdir"),
            &admin_contents,
            current_admin.as_deref(),
            &dot_git,
            &dot_git_contents,
        )?;
        Ok(true)
    }

    /// Return the trimmed lock reason, or `None` when the worktree is unlocked.
    /// An empty string represents a lock without a stated reason.
    ///
    /// # Errors
    /// Returns an error for an invalid name, missing registration, non-UTF-8
    /// lock content, or storage failure.
    pub fn worktree_lock_reason(&self, name: &str) -> Result<Option<String>> {
        let admin = self.worktree_admin_dir(name)?;
        let path = admin.join("locked");
        match self.filesystem().read(&path) {
            Ok(contents) => {
                let reason = std::str::from_utf8(&contents).map_err(|_| {
                    Error::InvalidRepository("non-UTF-8 worktree lock reason".into())
                })?;
                Ok(Some(reason.trim().to_owned()))
            }
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Lock a linked worktree so automatic pruning and ordinary removal retain
    /// its registration.
    ///
    /// The reason is stored in Git's `worktrees/<name>/locked` format.
    ///
    /// # Errors
    /// Returns an error for a missing registration, an existing lock, a reason
    /// containing NUL, or storage failure.
    pub fn lock_worktree(&self, name: &str, reason: Option<&str>) -> Result<()> {
        let admin = self.worktree_admin_dir(name)?;
        let reason = reason.unwrap_or("");
        if reason.contains('\0') {
            return Err(Error::InvalidRepository(
                "worktree lock reason contains NUL".into(),
            ));
        }
        let mut contents = reason.as_bytes().to_vec();
        contents.push(b'\n');
        self.filesystem()
            .write_new(&admin.join("locked"), &contents)
    }

    /// Unlock a linked worktree.
    ///
    /// # Errors
    /// Returns an error for an invalid or missing registration, an unlocked
    /// worktree, or storage failure.
    pub fn unlock_worktree(&self, name: &str) -> Result<()> {
        let admin = self.worktree_admin_dir(name)?;
        self.filesystem().remove_file(&admin.join("locked"))
    }

    /// Discover and optionally remove stale linked-worktree registrations.
    ///
    /// Only the administrative entry below `worktrees/` is removed. A
    /// `locked` file protects an entry, including a registration whose target
    /// is currently unavailable. The scan is bounded by `max_worktrees`.
    ///
    /// # Errors
    /// Returns an error for an exceeded scan bound, unreadable storage,
    /// invalid non-UTF-8 names, or a failed administrative removal.
    pub fn prune_worktrees(
        &self,
        options: &WorktreePruneOptions,
    ) -> Result<Vec<WorktreePruneEntry>> {
        let root = self.common_dir().join("worktrees");
        let names = match self.filesystem().read_dir(&root) {
            Ok(names) => names,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if names.len() > options.max_worktrees {
            return Err(Error::InvalidRepository(format!(
                "worktree count {} exceeds limit {}",
                names.len(),
                options.max_worktrees
            )));
        }

        let mut entries = Vec::new();
        let mut kept = vec![(
            self.common_dir().to_path_buf(),
            None::<String>,
            None::<PathBuf>,
        )];
        for name in names {
            let name_string = name
                .to_str()
                .ok_or_else(|| Error::InvalidRepository("non-UTF-8 worktree name".into()))?
                .to_owned();
            let admin = root.join(&name);
            let metadata = self.filesystem().metadata(&admin)?;
            let mut live_path = None;
            let reason = if !metadata.is_dir() {
                Some(WorktreePruneReason::NotDirectory)
            } else if self.filesystem().exists(&admin.join("locked"))? {
                None
            } else {
                match self.filesystem().read(&admin.join("gitdir")) {
                    Err(Error::NotFound(_)) => Some(WorktreePruneReason::MissingGitdir),
                    Err(error) => return Err(error),
                    Ok(contents) => match parse_path_file(&contents)
                        .and_then(|value| resolve_worktree_path(&admin, &value))
                    {
                        Err(_) => Some(WorktreePruneReason::InvalidGitdir),
                        Ok(dot_git) if self.filesystem().exists(&dot_git)? => {
                            live_path = Some(dot_git);
                            None
                        }
                        Ok(_) => {
                            let expired = match self.filesystem().metadata(&admin.join("index")) {
                                Ok(index) => {
                                    u64::from(index.stat().mtime_seconds) <= options.expire_before
                                }
                                Err(Error::NotFound(_)) => true,
                                Err(error) => return Err(error),
                            };
                            expired.then_some(WorktreePruneReason::MissingWorktree)
                        }
                    },
                }
            };
            if let Some(reason) = reason {
                entries.push(WorktreePruneEntry {
                    name: name_string,
                    path: admin,
                    reason,
                });
            } else if let Some(path) = live_path {
                kept.push((path, Some(name_string), Some(admin)));
            }
        }
        kept.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        for pair in kept.windows(2) {
            if pair[0].0 == pair[1].0 {
                let (_, Some(name), Some(path)) = &pair[1] else {
                    continue;
                };
                entries.push(WorktreePruneEntry {
                    name: name.clone(),
                    path: path.clone(),
                    reason: WorktreePruneReason::Duplicate,
                });
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        if !options.dry_run {
            for entry in &entries {
                remove_tree(self.filesystem(), entry.path())?;
            }
            match self.filesystem().remove_dir(&root) {
                Ok(()) | Err(Error::DirectoryNotEmpty(_) | Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(entries)
    }

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
        self.remove_worktree_with_options(
            name,
            &RemoveWorktreeOptions {
                force,
                override_lock: false,
            },
        )
    }

    /// Remove a linked worktree with independent dirty-state and lock
    /// authorities.
    ///
    /// # Errors
    /// Returns an error for a locked worktree without `override_lock`, a dirty
    /// worktree without `force`, invalid metadata, or storage failure.
    pub fn remove_worktree_with_options(
        &self,
        name: &str,
        options: &RemoveWorktreeOptions,
    ) -> Result<()> {
        validate_worktree_name(name)?;
        let admin = self.common_dir().join("worktrees").join(name);
        if self.worktree_lock_reason(name)?.is_some() && !options.override_lock {
            return Err(Error::ReferenceConflict(format!(
                "worktree `{name}` is locked"
            )));
        }
        let backlink = parse_path_file(&self.filesystem().read(&admin.join("gitdir"))?)?;
        let dot_git = resolve_relative(&admin, &backlink)?;
        let path = dot_git
            .parent()
            .ok_or_else(|| Error::InvalidRepository("invalid worktree backlink".into()))?
            .to_path_buf();
        if self.filesystem().exists(&path)? {
            if !options.force {
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

    fn worktree_admin_dir(&self, name: &str) -> Result<PathBuf> {
        validate_worktree_name(name)?;
        let admin = self.common_dir().join("worktrees").join(name);
        if !self.filesystem().metadata(&admin)?.is_dir() {
            return Err(Error::NotDirectory(admin));
        }
        Ok(admin)
    }

    fn reject_movable_worktree_submodules(&self, admin: &Path, source: &Path) -> Result<()> {
        if self.filesystem().exists(&admin.join("modules"))? {
            return Err(Error::InvalidRepository(
                "worktrees containing submodules cannot be moved".into(),
            ));
        }
        let index = match self.filesystem().read(&admin.join("index")) {
            Ok(contents) => crate::Index::parse(&contents)?,
            Err(Error::NotFound(_)) => crate::Index::default(),
            Err(error) => return Err(error),
        };
        for entry in index.entries() {
            if entry.stage() != 0 || entry.mode() != 0o160_000 {
                continue;
            }
            let path = std::str::from_utf8(entry.path()).map_err(|_| {
                Error::InvalidRepository("non-UTF-8 submodule path cannot be inspected".into())
            })?;
            if self.filesystem().exists(&source.join(path))? {
                return Err(Error::InvalidRepository(
                    "worktrees containing populated submodules cannot be moved".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_worktree_gitfile(&self, source: &Path, admin: &Path) -> Result<()> {
        let contents = self.filesystem().read(&source.join(".git"))?;
        let line = contents.strip_suffix(b"\n").unwrap_or(&contents);
        let value = line
            .strip_prefix(b"gitdir: ")
            .ok_or_else(|| Error::InvalidRepository("worktree .git file is malformed".into()))?;
        let path = parse_path_file(value)?;
        let resolved = resolve_worktree_path(source, &path)?;
        if resolved != admin {
            return Err(Error::InvalidRepository(
                "worktree .git file points at a different registration".into(),
            ));
        }
        Ok(())
    }

    fn remove_registered_move_destination(
        &self,
        moving_name: &str,
        destination: &Path,
        options: &MoveWorktreeOptions,
    ) -> Result<()> {
        let root = self.common_dir().join("worktrees");
        for entry in self.filesystem().read_dir(&root)? {
            let Some(name) = entry.to_str() else { continue };
            if name == moving_name {
                continue;
            }
            let admin = root.join(&entry);
            let Ok(contents) = self.filesystem().read(&admin.join("gitdir")) else {
                continue;
            };
            let Ok(backlink) = parse_path_file(&contents) else {
                continue;
            };
            let Ok(dot_git) = resolve_worktree_path(&admin, &backlink) else {
                continue;
            };
            if dot_git.parent() != Some(destination) {
                continue;
            }
            if !options.force_registered_destination {
                return Err(Error::ReferenceConflict(format!(
                    "destination is registered as worktree `{name}`"
                )));
            }
            if self.filesystem().exists(&admin.join("locked"))? && !options.override_locks {
                return Err(Error::ReferenceConflict(format!(
                    "destination worktree `{name}` is locked"
                )));
            }
            remove_tree(self.filesystem(), &admin)?;
        }
        Ok(())
    }

    pub(crate) fn ensure_branch_available(&self, branch: &ReferenceName) -> Result<()> {
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
        let mut config = self.read_config()?;
        if config.get("extensions.relativeworktrees")?.is_some()
            && config.get_bool("extensions.relativeworktrees")?
        {
            return Ok(());
        }
        if config.get_i64("core.repositoryformatversion")? != 0 {
            return Err(Error::InvalidRepository(
                "cannot enable relative worktrees for unknown repository format".into(),
            ));
        }
        config.set("core.repositoryformatversion", b"1")?;
        config.set("extensions.relativeworktrees", b"true")?;
        self.write_config(&config)
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

fn resolve_worktree_path(base: &Path, value: &Path) -> Result<PathBuf> {
    if value.is_absolute() {
        let mut path = PathBuf::new();
        for component in value.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(value) => path.push(value),
                _ => {
                    return Err(Error::InvalidRepository(
                        "invalid absolute worktree path".into(),
                    ));
                }
            }
        }
        if path.as_os_str().is_empty() {
            return Err(Error::InvalidRepository(
                "invalid absolute worktree path".into(),
            ));
        }
        Ok(path)
    } else {
        resolve_relative(base, value)
    }
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

fn optional_read(filesystem: &dyn crate::FileSystem, path: &Path) -> Result<Option<Vec<u8>>> {
    match filesystem.read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn publish_linking_files(
    filesystem: &dyn crate::FileSystem,
    admin_path: &Path,
    admin_contents: &[u8],
    old_admin_contents: Option<&[u8]>,
    dot_git_path: &Path,
    dot_git_contents: &[u8],
) -> Result<()> {
    let admin_lock = appended_lock_path(admin_path);
    let dot_git_lock = appended_lock_path(dot_git_path);
    filesystem.write_new(&admin_lock, admin_contents)?;
    if let Err(error) = filesystem.write_new(&dot_git_lock, dot_git_contents) {
        let _ = filesystem.remove_file(&admin_lock);
        return Err(error);
    }
    if let Err(error) = filesystem.rename(&admin_lock, admin_path) {
        let _ = filesystem.remove_file(&admin_lock);
        let _ = filesystem.remove_file(&dot_git_lock);
        return Err(error);
    }
    if let Err(error) = filesystem.rename(&dot_git_lock, dot_git_path) {
        let _ = filesystem.remove_file(&dot_git_lock);
        match old_admin_contents {
            Some(contents) => {
                let _ = filesystem.write(admin_path, contents);
            }
            None => {
                let _ = filesystem.remove_file(admin_path);
            }
        }
        return Err(error);
    }
    Ok(())
}

fn appended_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        AddWorktreeOptions, MoveWorktreeOptions, RemoveWorktreeOptions, WorktreePruneOptions,
        WorktreePruneReason, WorktreeTarget, remove_tree,
    };
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

        let config = repository.read_config().unwrap();
        assert_eq!(config.get_i64("core.repositoryformatversion").unwrap(), 1);
        assert!(config.get_bool("extensions.relativeworktrees").unwrap());

        filesystem
            .write(Path::new("topic-work/file"), b"dirty\n")
            .unwrap();
        assert!(repository.remove_worktree("topic-work", false).is_err());
        repository.remove_worktree("topic-work", true).unwrap();
        assert!(!filesystem.exists(Path::new("topic-work")).unwrap());
        assert!(repository.linked_worktrees().unwrap().is_empty());
    }

    #[test]
    fn locks_unlocks_and_requires_separate_lock_override_for_removal() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let tip = commit(&repository, None, b"main\n");
        repository
            .add_worktree(
                "detached-work",
                "detached-work",
                &WorktreeTarget::Detached(tip),
                &AddWorktreeOptions::default(),
            )
            .unwrap();

        repository
            .lock_worktree("detached-work", Some("  portable device  "))
            .unwrap();
        assert_eq!(
            filesystem
                .read(Path::new("main/.git/worktrees/detached-work/locked"))
                .unwrap(),
            b"  portable device  \n"
        );
        assert_eq!(
            repository.worktree_lock_reason("detached-work").unwrap(),
            Some("portable device".to_owned())
        );
        assert!(repository.lock_worktree("detached-work", None).is_err());
        assert!(repository.remove_worktree("detached-work", true).is_err());
        repository.unlock_worktree("detached-work").unwrap();
        assert_eq!(
            repository.worktree_lock_reason("detached-work").unwrap(),
            None
        );
        assert!(repository.unlock_worktree("detached-work").is_err());
        repository.lock_worktree("detached-work", None).unwrap();
        repository
            .remove_worktree_with_options(
                "detached-work",
                &RemoveWorktreeOptions {
                    force: false,
                    override_lock: true,
                },
            )
            .unwrap();
        assert!(!filesystem.exists(Path::new("detached-work")).unwrap());
    }

    #[test]
    fn repairs_both_links_after_an_external_worktree_move() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let tip = commit(&repository, None, b"main\n");
        repository
            .add_worktree(
                "old-place",
                "moved",
                &WorktreeTarget::Detached(tip),
                &AddWorktreeOptions {
                    checkout: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let old_dot_git = filesystem.read(Path::new("old-place/.git")).unwrap();
        filesystem.create_dir_all(Path::new("new-place")).unwrap();
        filesystem
            .write(Path::new("new-place/.git"), &old_dot_git)
            .unwrap();
        remove_tree(&filesystem, Path::new("old-place")).unwrap();

        let old_admin = filesystem
            .read(Path::new("main/.git/worktrees/moved/gitdir"))
            .unwrap();
        filesystem
            .write_new(Path::new("new-place/.git.lock"), b"busy")
            .unwrap();
        assert!(repository.repair_worktree("moved", "new-place").is_err());
        assert_eq!(
            filesystem
                .read(Path::new("main/.git/worktrees/moved/gitdir"))
                .unwrap(),
            old_admin
        );
        assert_eq!(
            filesystem.read(Path::new("new-place/.git")).unwrap(),
            old_dot_git
        );
        assert!(
            !filesystem
                .exists(Path::new("main/.git/worktrees/moved/gitdir.lock"))
                .unwrap()
        );
        filesystem
            .remove_file(Path::new("new-place/.git.lock"))
            .unwrap();

        assert!(repository.repair_worktree("moved", "new-place").unwrap());
        assert_eq!(
            filesystem.read(Path::new("new-place/.git")).unwrap(),
            b"gitdir: ../main/.git/worktrees/moved\n"
        );
        assert_eq!(
            filesystem
                .read(Path::new("main/.git/worktrees/moved/gitdir"))
                .unwrap(),
            b"../../../../new-place/.git\n"
        );
        assert!(!repository.repair_worktree("moved", "new-place").unwrap());
        assert_eq!(
            Repository::open(filesystem, "new-place")
                .unwrap()
                .resolve_reference("HEAD")
                .unwrap(),
            tip
        );
    }

    #[test]
    fn moves_locked_worktree_into_container_with_explicit_authority() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let tip = commit(&repository, None, b"move\n");
        repository
            .add_worktree(
                "old-place",
                "moving",
                &WorktreeTarget::Detached(tip),
                &AddWorktreeOptions::default(),
            )
            .unwrap();
        repository.lock_worktree("moving", Some("mounted")).unwrap();
        filesystem.create_dir_all(Path::new("container")).unwrap();

        assert!(
            repository
                .move_worktree("moving", "container", &MoveWorktreeOptions::default())
                .is_err()
        );
        let destination = repository
            .move_worktree(
                "moving",
                "container",
                &MoveWorktreeOptions {
                    override_locks: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(destination, Path::new("container/old-place"));
        assert!(!filesystem.exists(Path::new("old-place")).unwrap());
        assert_eq!(
            filesystem
                .read(Path::new("container/old-place/file"))
                .unwrap(),
            b"move\n"
        );
        assert_eq!(
            repository.linked_worktrees().unwrap()[0].path,
            Path::new("container/old-place")
        );
        assert_eq!(
            Repository::open(filesystem, "container/old-place")
                .unwrap()
                .resolve_reference("HEAD")
                .unwrap(),
            tip
        );
    }

    #[test]
    fn move_requires_force_to_replace_a_missing_registration() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let tip = commit(&repository, None, b"move\n");
        for (path, name) in [("source", "source"), ("destination", "stale")] {
            repository
                .add_worktree(
                    path,
                    name,
                    &WorktreeTarget::Detached(tip),
                    &AddWorktreeOptions::default(),
                )
                .unwrap();
        }
        remove_tree(&filesystem, Path::new("destination")).unwrap();
        assert!(
            repository
                .move_worktree("source", "destination", &MoveWorktreeOptions::default())
                .is_err()
        );
        repository
            .move_worktree(
                "source",
                "destination",
                &MoveWorktreeOptions {
                    force_registered_destination: true,
                    override_locks: false,
                },
            )
            .unwrap();
        assert!(
            !filesystem
                .exists(Path::new("main/.git/worktrees/stale"))
                .unwrap()
        );
        assert!(filesystem.exists(Path::new("destination/.git")).unwrap());
    }

    #[test]
    fn prunes_only_eligible_unlocked_administrative_entries() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        let root = Path::new("main/.git/worktrees");
        filesystem.create_dir_all(root).unwrap();

        let live = root.join("live");
        filesystem.create_dir_all(&live).unwrap();
        filesystem
            .write(&live.join("gitdir"), b"../../../../live/.git\n")
            .unwrap();
        filesystem.create_dir_all(Path::new("live")).unwrap();
        filesystem
            .write(Path::new("live/.git"), b"gitdir: elsewhere\n")
            .unwrap();

        let missing = root.join("missing");
        filesystem.create_dir_all(&missing).unwrap();
        filesystem
            .write(&missing.join("gitdir"), b"../../../../gone/.git\n")
            .unwrap();
        filesystem.write(&missing.join("index"), b"index").unwrap();

        let locked = root.join("locked");
        filesystem.create_dir_all(&locked).unwrap();
        filesystem
            .write(&locked.join("locked"), b"portable device\n")
            .unwrap();

        filesystem.create_dir_all(&root.join("malformed")).unwrap();
        filesystem
            .write(&root.join("stray"), b"not a directory")
            .unwrap();
        let duplicate = root.join("live-copy");
        filesystem.create_dir_all(&duplicate).unwrap();
        filesystem
            .write(&duplicate.join("gitdir"), b"../../../../live/.git\n")
            .unwrap();

        let options = WorktreePruneOptions {
            expire_before: 0,
            ..Default::default()
        };
        let entries = repository.prune_worktrees(&options).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name(), "live-copy");
        assert_eq!(entries[0].reason(), WorktreePruneReason::Duplicate);
        assert_eq!(entries[1].name(), "malformed");
        assert_eq!(entries[1].reason(), WorktreePruneReason::MissingGitdir);
        assert_eq!(entries[2].name(), "missing");
        assert_eq!(entries[2].reason(), WorktreePruneReason::MissingWorktree);
        assert_eq!(entries[3].reason(), WorktreePruneReason::NotDirectory);
        assert!(filesystem.exists(&missing).unwrap());

        let removed = repository
            .prune_worktrees(&WorktreePruneOptions {
                dry_run: false,
                ..options
            })
            .unwrap();
        assert_eq!(removed, entries);
        assert!(filesystem.exists(&live).unwrap());
        assert!(!filesystem.exists(&duplicate).unwrap());
        assert!(filesystem.exists(&locked).unwrap());
        assert!(!filesystem.exists(&missing).unwrap());
        assert!(!filesystem.exists(&root.join("malformed")).unwrap());
        assert!(!filesystem.exists(&root.join("stray")).unwrap());
    }

    #[test]
    fn bounds_worktree_prune_scans() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "main", &InitOptions::default()).unwrap();
        filesystem
            .create_dir_all(Path::new("main/.git/worktrees/one"))
            .unwrap();
        let error = repository
            .prune_worktrees(&WorktreePruneOptions {
                max_worktrees: 0,
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
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
