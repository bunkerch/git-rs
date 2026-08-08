//! Gitlink-backed submodule discovery, initialization, status, and update.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::worktree::{has_symlink_leading_path, worktree_path};
use crate::{
    CheckoutOptions, CloneOptions, Config, Error, FetchOptions, IndexEntry, ObjectId, Repository,
    Result, StatData, UploadPackTransport,
};

/// Bounds and optional literal path selection for metadata/status/init.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleOptions {
    pub paths: Vec<Vec<u8>>,
    pub cached: bool,
    pub max_modules: usize,
    pub max_gitmodules_size: usize,
}

impl Default for SubmoduleOptions {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            cached: false,
            max_modules: 1_000_000,
            max_gitmodules_size: 64 * 1024 * 1024,
        }
    }
}

/// One validated `[submodule "name"]` definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Submodule {
    name: Vec<u8>,
    path: Vec<u8>,
    url: Vec<u8>,
    branch: Option<Vec<u8>>,
    update: Option<Vec<u8>>,
}

impl Submodule {
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
    #[must_use]
    pub fn url(&self) -> &[u8] {
        &self.url
    }
    #[must_use]
    pub fn branch(&self) -> Option<&[u8]> {
        self.branch.as_deref()
    }
    #[must_use]
    pub fn update(&self) -> Option<&[u8]> {
        self.update.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmoduleStatusKind {
    Uninitialized,
    Matches,
    Different,
    Conflict,
}

/// Superproject gitlink versus nested repository state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleStatus {
    module: Submodule,
    expected: Option<ObjectId>,
    actual: Option<ObjectId>,
    kind: SubmoduleStatusKind,
}

impl SubmoduleStatus {
    #[must_use]
    pub const fn module(&self) -> &Submodule {
        &self.module
    }
    #[must_use]
    pub const fn expected(&self) -> Option<ObjectId> {
        self.expected
    }
    #[must_use]
    pub const fn actual(&self) -> Option<ObjectId> {
        self.actual
    }
    #[must_use]
    pub const fn kind(&self) -> SubmoduleStatusKind {
        self.kind
    }
    #[must_use]
    pub const fn prefix(&self) -> char {
        match self.kind {
            SubmoduleStatusKind::Uninitialized => '-',
            SubmoduleStatusKind::Matches => ' ',
            SubmoduleStatusKind::Different => '+',
            SubmoduleStatusKind::Conflict => 'U',
        }
    }
}

/// Checkout update behavior and protocol/object bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleUpdateOptions {
    pub init: bool,
    pub force: bool,
    pub max_modules: usize,
    pub max_gitmodules_size: usize,
    pub max_pack_size: usize,
    pub max_object_size: usize,
    pub max_total_inflated_size: usize,
}

impl Default for SubmoduleUpdateOptions {
    fn default() -> Self {
        Self {
            init: false,
            force: false,
            max_modules: 1_000_000,
            max_gitmodules_size: 64 * 1024 * 1024,
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleUpdateReport {
    pub cloned: bool,
    pub fetched_objects: usize,
    pub previous: Option<ObjectId>,
    pub current: ObjectId,
}

/// Metadata and transfer bounds for adding one new submodule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleAddOptions {
    pub name: Option<Vec<u8>>,
    pub branch: Option<Vec<u8>>,
    pub max_gitmodules_size: usize,
    pub max_pack_size: usize,
    pub max_object_size: usize,
    pub max_total_inflated_size: usize,
}

impl Default for SubmoduleAddOptions {
    fn default() -> Self {
        Self {
            name: None,
            branch: None,
            max_gitmodules_size: 64 * 1024 * 1024,
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Result of cloning and staging a new submodule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleAddReport {
    pub module: Submodule,
    pub gitlink: ObjectId,
    pub fetched_objects: usize,
}

/// Worktree safety and traversal bounds for submodule deinitialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleDeinitOptions {
    pub force: bool,
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_object_size: usize,
    pub max_modules: usize,
    pub max_gitmodules_size: usize,
}

impl Default for SubmoduleDeinitOptions {
    fn default() -> Self {
        Self {
            force: false,
            max_entries: 10_000_000,
            max_depth: 4096,
            max_object_size: 1024 * 1024 * 1024,
            max_modules: 1_000_000,
            max_gitmodules_size: 64 * 1024 * 1024,
        }
    }
}

/// Result of unregistering one submodule while retaining its object store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleDeinitReport {
    pub module: Submodule,
    pub was_populated: bool,
    pub removed_entries: usize,
}

impl Repository {
    /// Parse and validate `.gitmodules` from the superproject worktree.
    ///
    /// # Errors
    /// Returns an error for bare repositories, missing/non-file/oversized or
    /// malformed config, duplicate names/paths/required keys, unsafe paths,
    /// exceeded module limits, or storage failures.
    pub fn submodules(&self, options: &SubmoduleOptions) -> Result<Vec<Submodule>> {
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let path = worktree.join(".gitmodules");
        let metadata = self.filesystem().metadata(&path)?;
        if !metadata.is_file() || metadata.len() > options.max_gitmodules_size as u64 {
            return if metadata.is_file() {
                Err(Error::ObjectTooLarge {
                    declared: metadata.len(),
                    limit: options.max_gitmodules_size,
                })
            } else {
                Err(Error::InvalidRepository(
                    ".gitmodules is not a regular file".into(),
                ))
            };
        }
        parse_modules(&self.filesystem().read(&path)?, options)
    }

    /// Compare configured gitlinks with the index and nested repositories.
    ///
    /// # Errors
    /// Returns metadata errors plus malformed index/gitlinks, unsafe nested
    /// repository state, or storage failures.
    pub fn submodule_status(&self, options: &SubmoduleOptions) -> Result<Vec<SubmoduleStatus>> {
        let modules = self.submodules(options)?;
        let index = self.read_index()?;
        let mut output = Vec::with_capacity(modules.len());
        for module in modules {
            let matching = index
                .entries()
                .iter()
                .filter(|entry| entry.path() == module.path())
                .collect::<Vec<_>>();
            let conflict = matching.iter().any(|entry| entry.stage() != 0);
            let expected = matching
                .iter()
                .find(|entry| entry.stage() == 0 && entry.mode() == 0o160_000)
                .map(|entry| entry.id());
            if !conflict && expected.is_none() {
                return Err(Error::InvalidRepository(format!(
                    "submodule `{}` has no stage-zero gitlink",
                    String::from_utf8_lossy(module.path())
                )));
            }
            let (actual, kind) = if conflict {
                (None, SubmoduleStatusKind::Conflict)
            } else if options.cached {
                (expected, SubmoduleStatusKind::Matches)
            } else {
                match self.open_submodule(&module) {
                    Ok(repository) => match repository.resolve_reference("HEAD") {
                        Ok(id) => (
                            Some(id),
                            if Some(id) == expected {
                                SubmoduleStatusKind::Matches
                            } else {
                                SubmoduleStatusKind::Different
                            },
                        ),
                        Err(Error::NotFound(_)) => (None, SubmoduleStatusKind::Different),
                        Err(error) => return Err(error),
                    },
                    Err(Error::NotFound(_)) => (None, SubmoduleStatusKind::Uninitialized),
                    Err(error) => return Err(error),
                }
            };
            output.push(SubmoduleStatus {
                module,
                expected,
                actual,
                kind,
            });
        }
        Ok(output)
    }

    /// Copy selected `.gitmodules` URL/update settings into local config.
    ///
    /// Existing local values are retained. Custom `!command` update values are
    /// never copied.
    ///
    /// # Errors
    /// Returns metadata/config validation, selection, lock, or storage errors.
    pub fn init_submodules(&self, options: &SubmoduleOptions) -> Result<Vec<Vec<u8>>> {
        let modules = self.submodules(options)?;
        let mut config = self.read_config()?;
        let mut initialized = Vec::new();
        for module in modules {
            if subsection_value(&config, module.name(), "url").is_none() {
                config.set_in_subsection("submodule", module.name(), "url", module.url())?;
            }
            if let Some(update) = module.update()
                && !update.starts_with(b"!")
                && subsection_value(&config, module.name(), "update").is_none()
            {
                config.set_in_subsection("submodule", module.name(), "update", update)?;
            }
            initialized.push(module.path);
        }
        self.write_config(&config)?;
        Ok(initialized)
    }

    /// Clone, register, and stage one new submodule.
    ///
    /// The supplied transport corresponds to `url`. The clone is stored below
    /// the superproject's common `modules/` directory and the worktree receives
    /// a relative `.git` indirection, matching modern Git's absorbed layout.
    ///
    /// # Errors
    /// Returns an error for unsafe/duplicate names or paths, a nonempty target,
    /// an existing administrative repository, malformed metadata/index state,
    /// unavailable branches, transport/object failures, or storage failures.
    pub fn add_submodule<T: UploadPackTransport>(
        &self,
        path: &[u8],
        url: &[u8],
        transport: &mut T,
        options: &SubmoduleAddOptions,
    ) -> Result<SubmoduleAddReport> {
        validate_submodule_path(path)?;
        let name = options.name.as_deref().unwrap_or(path);
        validate_submodule_path(name)?;
        if url.is_empty() || url.contains(&0) {
            return Err(Error::InvalidRepository("invalid submodule URL".into()));
        }
        let (mut modules_config, existing) = self.read_gitmodules_for_add(options)?;
        if existing
            .iter()
            .any(|module| paths_overlap(module.name(), name) || module.path() == path)
        {
            return Err(Error::InvalidRepository(
                "submodule name or path is already registered".into(),
            ));
        }
        let index = self.read_index()?;
        if index
            .entries()
            .iter()
            .any(|entry| paths_overlap(entry.path(), path))
        {
            return Err(Error::InvalidRepository(
                "submodule path overlaps an index entry".into(),
            ));
        }
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let relative = worktree_path(path)?;
        if has_symlink_leading_path(self.filesystem(), worktree, &relative)? {
            return Err(Error::BeyondSymbolicLink(relative));
        }
        let nested_path = worktree.join(relative);
        let transfer = add_transfer_options(options);
        let (nested, _, fetched_objects) =
            self.clone_submodule(&nested_path, name, url, transport, &transfer, false)?;
        let (gitlink, checkout_branch) = self.add_submodule_target(&nested, options)?;
        let commit = nested.read_commit(gitlink, options.max_object_size)?;
        nested.checkout_tree(
            commit.tree(),
            &CheckoutOptions {
                force: true,
                max_object_size: options.max_object_size,
            },
        )?;
        if let Some(branch) = checkout_branch.as_deref() {
            nested.write_atomic(
                Path::new("HEAD"),
                format!("ref: refs/heads/{branch}\n").as_bytes(),
            )?;
        }

        modules_config.set_in_subsection("submodule", name, "path", path)?;
        modules_config.set_in_subsection("submodule", name, "url", url)?;
        if let Some(branch) = &options.branch {
            modules_config.set_in_subsection("submodule", name, "branch", branch)?;
        }
        let encoded = modules_config.encode();
        if encoded.len() > options.max_gitmodules_size {
            return Err(Error::ObjectTooLarge {
                declared: u64::try_from(encoded.len()).unwrap_or(u64::MAX),
                limit: options.max_gitmodules_size,
            });
        }
        self.filesystem()
            .write(&worktree.join(".gitmodules"), &encoded)?;
        let mut local = self.read_config()?;
        local.set_in_subsection("submodule", name, "url", url)?;
        local.set_in_subsection("submodule", name, "active", b"true")?;
        self.write_config(&local)?;
        self.add(".gitmodules")?;
        let current = self.read_index()?;
        let mut entries = current.entries().to_vec();
        entries.push(IndexEntry::new(
            path.to_vec(),
            0o160_000,
            gitlink,
            StatData::default(),
        )?);
        self.write_index(&current.with_entries(entries)?)?;
        Ok(SubmoduleAddReport {
            module: Submodule {
                name: name.to_vec(),
                path: path.to_vec(),
                url: url.to_vec(),
                branch: options.branch.clone(),
                update: None,
            },
            gitlink,
            fetched_objects,
        })
    }

    /// Remove one submodule worktree and unregister its local configuration.
    ///
    /// The administrative repository below the superproject common directory
    /// is retained. A legacy embedded `.git` directory is absorbed there before
    /// the worktree is cleared.
    ///
    /// # Errors
    /// Returns an error for an unknown path, local modifications without
    /// `force`, unsafe/corrupt repository state, exceeded traversal bounds, or
    /// storage failures.
    pub fn deinit_submodule(
        &self,
        path: &[u8],
        options: &SubmoduleDeinitOptions,
    ) -> Result<SubmoduleDeinitReport> {
        let selection = SubmoduleOptions {
            paths: vec![path.to_vec()],
            max_modules: options.max_modules,
            max_gitmodules_size: options.max_gitmodules_size,
            ..SubmoduleOptions::default()
        };
        let selected_path = worktree_path(path)?;
        let module = self
            .submodules(&selection)?
            .into_iter()
            .next()
            .ok_or(Error::NotFound(selected_path))?;
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let relative = worktree_path(module.path())?;
        if has_symlink_leading_path(self.filesystem(), worktree, &relative)? {
            return Err(Error::BeyondSymbolicLink(relative));
        }
        let nested_path = worktree.join(relative);
        let populated = self.filesystem().exists(&nested_path.join(".git"))?;
        if populated {
            let nested = Repository::open_shared(self.shared_filesystem(), &nested_path)?;
            if !options.force
                && !nested
                    .status(&crate::StatusOptions {
                        include_untracked: true,
                        max_object_size: options.max_object_size,
                    })?
                    .is_clean()
            {
                return Err(Error::InvalidRepository(
                    "submodule worktree contains local modifications".into(),
                ));
            }
            self.absorb_submodule_gitdir(&nested_path, module.name())?;
        }
        let removed_entries = if self.filesystem().exists(&nested_path)? {
            clear_directory(
                self.filesystem(),
                &nested_path,
                options.max_entries,
                options.max_depth,
            )?
        } else {
            self.filesystem().create_dir_all(&nested_path)?;
            0
        };
        let admin = self.submodule_admin_path(module.name())?;
        if self.filesystem().exists(&admin.join("HEAD"))? {
            let administrative = Repository::open_shared(self.shared_filesystem(), &admin)?;
            let mut config = administrative.read_config()?;
            config.unset("core.worktree")?;
            administrative.write_config(&config)?;
        }
        let mut config = self.read_config()?;
        config.remove_subsection("submodule", module.name())?;
        self.write_config(&config)?;
        Ok(SubmoduleDeinitReport {
            module,
            was_populated: populated,
            removed_entries,
        })
    }

    /// Synchronize initialized local and nested-remote URLs from `.gitmodules`.
    ///
    /// Relative URLs are resolved against the superproject's default remote;
    /// nested repositories receive the additional path back to the
    /// superproject, matching Git's command-line behavior.
    ///
    /// # Errors
    /// Returns metadata/config validation, unsafe relative URL, repository, or
    /// storage errors.
    pub fn sync_submodules(&self, options: &SubmoduleOptions) -> Result<Vec<Vec<u8>>> {
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let modules = self.submodules(options)?;
        let mut super_config = self.read_config()?;
        let base = default_remote_url(self, &super_config)?;
        let mut synchronized = Vec::new();
        for module in modules {
            if subsection_value(&super_config, module.name(), "url").is_none() {
                continue;
            }
            let relative = worktree_path(module.path())?;
            if has_symlink_leading_path(self.filesystem(), worktree, &relative)? {
                return Err(Error::BeyondSymbolicLink(relative));
            }
            let super_url = resolve_relative_url(&base, module.url(), None)?;
            super_config.set_in_subsection("submodule", module.name(), "url", &super_url)?;
            match self.open_submodule(&module) {
                Ok(nested) => {
                    let mut nested_config = nested.read_config()?;
                    let remote = default_remote_name(&nested, &nested_config)?;
                    let up = b"../".repeat(module.path().split(|byte| *byte == b'/').count());
                    let nested_url = resolve_relative_url(&base, module.url(), Some(&up))?;
                    nested_config.set_in_subsection(
                        "remote",
                        remote.as_bytes(),
                        "url",
                        &nested_url,
                    )?;
                    nested.write_config(&nested_config)?;
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            synchronized.push(module.path);
        }
        self.write_config(&super_config)?;
        Ok(synchronized)
    }

    /// Clone/fetch one initialized submodule and detach it at its index gitlink.
    ///
    /// The caller provides the transport corresponding to the configured URL;
    /// no URL scheme or process is assumed by the library.
    ///
    /// # Errors
    /// Returns an error for unknown/uninitialized/conflicted submodules,
    /// unsupported update policy, unsafe paths, transport/protocol/object
    /// failures, checkout conflicts, or storage failures.
    pub fn update_submodule<T: UploadPackTransport>(
        &self,
        path: &[u8],
        transport: &mut T,
        options: &SubmoduleUpdateOptions,
    ) -> Result<SubmoduleUpdateReport> {
        let selection = SubmoduleOptions {
            paths: vec![path.to_vec()],
            max_modules: options.max_modules,
            max_gitmodules_size: options.max_gitmodules_size,
            ..SubmoduleOptions::default()
        };
        if options.init {
            self.init_submodules(&selection)?;
        }
        let selected_path = worktree_path(path)?;
        let module = self
            .submodules(&selection)?
            .into_iter()
            .next()
            .ok_or(Error::NotFound(selected_path))?;
        let config = self.read_config()?;
        let url = subsection_value(&config, module.name(), "url").ok_or_else(|| {
            Error::InvalidRepository(format!(
                "submodule `{}` is not initialized",
                String::from_utf8_lossy(module.name())
            ))
        })?;
        let update = subsection_value(&config, module.name(), "update")
            .or(module.update())
            .unwrap_or(b"checkout");
        if update != b"checkout" {
            return Err(Error::InvalidRepository(
                "only checkout submodule update policy is valid for detached update".into(),
            ));
        }
        let index = self.read_index()?;
        if index
            .entries()
            .iter()
            .any(|entry| entry.path() == module.path() && entry.stage() != 0)
        {
            return Err(Error::InvalidRepository(
                "cannot update conflicted submodule gitlink".into(),
            ));
        }
        let expected = index
            .entries()
            .iter()
            .find(|entry| {
                entry.path() == module.path() && entry.stage() == 0 && entry.mode() == 0o160_000
            })
            .map(crate::IndexEntry::id)
            .ok_or_else(|| {
                Error::InvalidRepository("submodule has no stage-zero gitlink".into())
            })?;
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let relative = worktree_path(module.path())?;
        if has_symlink_leading_path(self.filesystem(), worktree, &relative)? {
            return Err(Error::BeyondSymbolicLink(relative));
        }
        let nested_path = worktree.join(relative);
        let (nested, cloned, fetched_objects) =
            self.obtain_submodule_repository(&nested_path, module.name(), url, transport, options)?;
        nested.read_commit(expected, options.max_object_size)?;
        let previous = nested.resolve_reference("HEAD").ok();
        if cloned || previous != Some(expected) || options.force {
            let commit = nested.read_commit(expected, options.max_object_size)?;
            nested.checkout_tree(
                commit.tree(),
                &CheckoutOptions {
                    force: options.force || cloned,
                    max_object_size: options.max_object_size,
                },
            )?;
            nested.write_atomic(Path::new("HEAD"), format!("{expected}\n").as_bytes())?;
        }
        Ok(SubmoduleUpdateReport {
            cloned,
            fetched_objects,
            previous,
            current: expected,
        })
    }

    fn open_submodule(&self, module: &Submodule) -> Result<Repository> {
        let root = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        let nested = root.join(worktree_path(module.path())?);
        if !self.filesystem().exists(&nested.join(".git"))? {
            return Err(Error::NotFound(nested));
        }
        Repository::open_shared(self.shared_filesystem(), nested)
    }

    fn read_gitmodules_for_add(
        &self,
        options: &SubmoduleAddOptions,
    ) -> Result<(Config, Vec<Submodule>)> {
        let worktree = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?;
        match self.filesystem().read(&worktree.join(".gitmodules")) {
            Ok(data) => {
                if data.len() > options.max_gitmodules_size {
                    return Err(Error::ObjectTooLarge {
                        declared: u64::try_from(data.len()).unwrap_or(u64::MAX),
                        limit: options.max_gitmodules_size,
                    });
                }
                let config = Config::parse(&data)?;
                let modules = parse_modules(
                    &data,
                    &SubmoduleOptions {
                        max_gitmodules_size: options.max_gitmodules_size,
                        ..SubmoduleOptions::default()
                    },
                )?;
                Ok((config, modules))
            }
            Err(Error::NotFound(_)) => Ok((Config::parse(b"")?, Vec::new())),
            Err(error) => Err(error),
        }
    }

    fn add_submodule_target(
        &self,
        nested: &Repository,
        options: &SubmoduleAddOptions,
    ) -> Result<(ObjectId, Option<String>)> {
        let Some(branch) = options.branch.as_deref() else {
            return Ok((nested.resolve_reference("HEAD")?, None));
        };
        let branch = if branch == b"." {
            let head = self.read_reference("HEAD")?;
            let crate::ReferenceTarget::Symbolic(target) = head.target() else {
                return Err(Error::InvalidRepository(
                    "submodule branch `.` requires a symbolic superproject HEAD".into(),
                ));
            };
            target
                .as_str()
                .strip_prefix("refs/heads/")
                .ok_or_else(|| Error::InvalidRepository("HEAD is not a local branch".into()))?
                .to_owned()
        } else {
            std::str::from_utf8(branch)
                .map_err(|_| Error::InvalidRepository("submodule branch is not UTF-8".into()))?
                .to_owned()
        };
        let reference = crate::ReferenceName::branch(&branch)?;
        Ok((nested.resolve_reference(reference.as_str())?, Some(branch)))
    }

    fn submodule_admin_path(&self, name: &[u8]) -> Result<PathBuf> {
        let modules_root = self.common_dir().join("modules");
        let name_path = worktree_path(name)?;
        let mut prefix = modules_root.clone();
        let components = name_path.components().collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            prefix.push(component.as_os_str());
            if self.filesystem().exists(&prefix.join("HEAD"))? {
                return Err(Error::InvalidRepository(
                    "submodule administrative directory would nest inside another repository"
                        .into(),
                ));
            }
        }
        Ok(modules_root.join(name_path))
    }

    fn absorb_submodule_gitdir(&self, nested_path: &Path, name: &[u8]) -> Result<()> {
        let dot_git = nested_path.join(".git");
        if !self.filesystem().metadata(&dot_git)?.is_dir() {
            return Ok(());
        }
        let admin = self.submodule_admin_path(name)?;
        if self.filesystem().exists(&admin)? {
            return Err(Error::AlreadyExists(admin));
        }
        if let Some(parent) = admin.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        self.filesystem().rename(&dot_git, &admin)?;
        let mut config =
            Repository::open_shared(self.shared_filesystem(), &admin)?.read_config()?;
        config.set("core.bare", b"false")?;
        config.set(
            "core.worktree",
            path_bytes(&relative_path(&admin, nested_path)?)?,
        )?;
        let administrative = Repository::open_shared(self.shared_filesystem(), &admin)?;
        administrative.write_config(&config)?;
        let pointer = relative_path(nested_path, &admin)?;
        self.filesystem().write(
            &dot_git,
            format!("gitdir: {}\n", pointer.display()).as_bytes(),
        )
    }

    fn obtain_submodule_repository<T: UploadPackTransport>(
        &self,
        nested_path: &Path,
        name: &[u8],
        url: &[u8],
        transport: &mut T,
        options: &SubmoduleUpdateOptions,
    ) -> Result<(Repository, bool, usize)> {
        if !self.filesystem().exists(&nested_path.join(".git"))? {
            return self.clone_submodule(nested_path, name, url, transport, options, true);
        }
        match Repository::open_shared(self.shared_filesystem(), nested_path) {
            Ok(repository) => {
                let fetched = repository.fetch(transport, &fetch_options(options))?;
                Ok((repository, false, fetched.received_objects))
            }
            Err(error) => Err(error),
        }
    }

    fn clone_submodule<T: UploadPackTransport>(
        &self,
        nested_path: &Path,
        name: &[u8],
        url: &[u8],
        transport: &mut T,
        options: &SubmoduleUpdateOptions,
        reuse_admin: bool,
    ) -> Result<(Repository, bool, usize)> {
        let url = std::str::from_utf8(url)
            .map_err(|_| Error::InvalidRepository("submodule URL is not UTF-8".into()))?;
        let admin = self.submodule_admin_path(name)?;
        if self.filesystem().exists(&admin)? {
            if !reuse_admin || !self.filesystem().exists(&admin.join("HEAD"))? {
                return Err(Error::AlreadyExists(admin));
            }
            if self.filesystem().exists(nested_path)?
                && !self.filesystem().read_dir(nested_path)?.is_empty()
            {
                return Err(Error::InvalidRepository(format!(
                    "submodule worktree `{}` is not empty",
                    nested_path.display()
                )));
            }
            let administrative = Repository::open_shared(self.shared_filesystem(), &admin)?;
            let fetched = administrative.fetch(transport, &fetch_options(options))?;
            self.connect_submodule_worktree(&administrative, nested_path, &admin)?;
            let repository = Repository::open_shared(self.shared_filesystem(), nested_path)?;
            return Ok((repository, true, fetched.received_objects));
        }
        if self.filesystem().exists(nested_path)?
            && !self.filesystem().read_dir(nested_path)?.is_empty()
        {
            return Err(Error::InvalidRepository(format!(
                "submodule worktree `{}` is not empty",
                nested_path.display()
            )));
        }
        let (administrative, fetched) = Repository::clone_from_shared(
            self.shared_filesystem(),
            &admin,
            transport,
            &CloneOptions {
                remote_url: url.to_owned(),
                bare: true,
                checkout: false,
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
                ..CloneOptions::default()
            },
        )?;
        self.connect_submodule_worktree(&administrative, nested_path, &admin)?;
        let repository = Repository::open_shared(self.shared_filesystem(), nested_path)?;
        Ok((repository, true, fetched.received_objects))
    }

    fn connect_submodule_worktree(
        &self,
        administrative: &Repository,
        nested_path: &Path,
        admin: &Path,
    ) -> Result<()> {
        let mut config = administrative.read_config()?;
        config.set("core.bare", b"false")?;
        config.set(
            "core.worktree",
            path_bytes(&relative_path(admin, nested_path)?)?,
        )?;
        administrative.write_config(&config)?;
        self.filesystem().create_dir_all(nested_path)?;
        let pointer = relative_path(nested_path, admin)?;
        self.filesystem().write(
            &nested_path.join(".git"),
            format!("gitdir: {}\n", pointer.display()).as_bytes(),
        )
    }
}

fn fetch_options(options: &SubmoduleUpdateOptions) -> FetchOptions {
    FetchOptions {
        max_pack_size: options.max_pack_size,
        max_object_size: options.max_object_size,
        max_total_inflated_size: options.max_total_inflated_size,
        ..FetchOptions::default()
    }
}

fn add_transfer_options(options: &SubmoduleAddOptions) -> SubmoduleUpdateOptions {
    SubmoduleUpdateOptions {
        max_gitmodules_size: options.max_gitmodules_size,
        max_pack_size: options.max_pack_size,
        max_object_size: options.max_object_size,
        max_total_inflated_size: options.max_total_inflated_size,
        ..SubmoduleUpdateOptions::default()
    }
}

fn paths_overlap(left: &[u8], right: &[u8]) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
}

fn default_remote_name(repository: &Repository, config: &Config) -> Result<String> {
    let head = repository.read_reference("HEAD")?;
    let branch = match head.target() {
        crate::ReferenceTarget::Symbolic(target) => target
            .as_str()
            .strip_prefix("refs/heads/")
            .map(str::as_bytes),
        crate::ReferenceTarget::Direct(_) => None,
    };
    let configured =
        branch.and_then(|branch| config_value_in_subsection(config, "branch", branch, "remote"));
    match configured {
        Some(b".") | None => Ok("origin".to_owned()),
        Some(value) => std::str::from_utf8(value)
            .map(str::to_owned)
            .map_err(|_| Error::InvalidRepository("default remote is not UTF-8".into())),
    }
}

fn default_remote_url(repository: &Repository, config: &Config) -> Result<Vec<u8>> {
    let remote = default_remote_name(repository, config)?;
    Ok(
        config_value_in_subsection(config, "remote", remote.as_bytes(), "url")
            .unwrap_or(b".")
            .to_vec(),
    )
}

fn resolve_relative_url(base: &[u8], relative: &[u8], up: Option<&[u8]>) -> Result<Vec<u8>> {
    if !(relative.starts_with(b"./") || relative.starts_with(b"../")) {
        return Ok(relative.to_vec());
    }
    if base.is_empty() || base.contains(&0) || relative.contains(&0) {
        return Err(Error::InvalidRepository(
            "invalid relative submodule URL".into(),
        ));
    }
    let mut remote = base.strip_suffix(b"/").unwrap_or(base).to_vec();
    let is_relative = !remote.starts_with(b"/") && !remote.contains(&b':');
    if is_relative && !(remote.starts_with(b"./") || remote.starts_with(b"../")) {
        remote.splice(0..0, b"./".iter().copied());
    }
    let mut value = relative;
    let mut colon_separator = false;
    loop {
        if let Some(rest) = value.strip_prefix(b"../") {
            value = rest;
            if let Some(position) = remote.iter().rposition(|byte| *byte == b'/') {
                remote.truncate(position);
            } else if let Some(position) = remote.iter().rposition(|byte| *byte == b':') {
                remote.truncate(position);
                colon_separator = true;
            } else if is_relative || remote == b"." {
                return Err(Error::InvalidRepository(
                    "relative submodule URL escapes its remote".into(),
                ));
            } else {
                remote.clear();
                remote.push(b'.');
            }
        } else if let Some(rest) = value.strip_prefix(b"./") {
            value = rest;
        } else {
            break;
        }
    }
    remote.push(if colon_separator { b':' } else { b'/' });
    remote.extend_from_slice(value);
    if value.ends_with(b"/") {
        remote.pop();
    }
    let resolved = remote.strip_prefix(b"./").unwrap_or(&remote);
    if is_relative && let Some(up) = up {
        let mut nested = up.to_vec();
        nested.extend_from_slice(resolved);
        return Ok(nested);
    }
    Ok(resolved.to_vec())
}

fn relative_path(from: &Path, to: &Path) -> Result<PathBuf> {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    if from.first().is_some_and(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        )
    }) != to.first().is_some_and(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(Error::InvalidRepository(
            "submodule paths use different storage roots".into(),
        ));
    }
    let mut output = PathBuf::new();
    for _ in common..from.len() {
        output.push("..");
    }
    for component in &to[common..] {
        output.push(component.as_os_str());
    }
    if output.as_os_str().is_empty() {
        output.push(".");
    }
    Ok(output)
}

fn path_bytes(path: &Path) -> Result<&[u8]> {
    path.to_str()
        .map(str::as_bytes)
        .ok_or_else(|| Error::InvalidRepository("submodule path is not UTF-8".into()))
}

fn clear_directory(
    filesystem: &dyn crate::FileSystem,
    root: &Path,
    max_entries: usize,
    max_depth: usize,
) -> Result<usize> {
    if !filesystem.metadata(root)?.is_dir() {
        return Err(Error::InvalidRepository(
            "submodule worktree is not a directory".into(),
        ));
    }
    let mut removed = 0usize;
    let mut stack = vec![(root.to_path_buf(), false, 0usize)];
    while let Some((path, visited, depth)) = stack.pop() {
        let metadata = filesystem.metadata(&path)?;
        if metadata.is_dir() {
            if depth > max_depth {
                return Err(Error::InvalidRepository(
                    "submodule worktree exceeds depth limit".into(),
                ));
            }
            if visited {
                if path != root {
                    filesystem.remove_dir(&path)?;
                }
                continue;
            }
            stack.push((path.clone(), true, depth));
            let children = filesystem.read_dir(&path)?;
            removed = removed.checked_add(children.len()).ok_or_else(|| {
                Error::InvalidRepository("submodule worktree entry count overflow".into())
            })?;
            if removed > max_entries {
                return Err(Error::InvalidRepository(
                    "submodule worktree exceeds entry limit".into(),
                ));
            }
            for child in children.into_iter().rev() {
                stack.push((path.join(child), false, depth.saturating_add(1)));
            }
        } else {
            filesystem.remove_file(&path)?;
        }
    }
    Ok(removed)
}

#[derive(Default)]
struct ModuleBuilder {
    path: Option<Vec<u8>>,
    url: Option<Vec<u8>>,
    branch: Option<Vec<u8>>,
    update: Option<Vec<u8>>,
}

fn parse_modules(data: &[u8], options: &SubmoduleOptions) -> Result<Vec<Submodule>> {
    let config = Config::parse(data)?;
    let mut builders = BTreeMap::<Vec<u8>, ModuleBuilder>::new();
    for entry in config.entries() {
        if entry.section() != "submodule" {
            continue;
        }
        let name = entry
            .subsection()
            .ok_or_else(|| Error::InvalidRepository("submodule section has no name".into()))?
            .to_vec();
        let value = entry
            .value()
            .ok_or_else(|| Error::InvalidRepository("implicit submodule value".into()))?
            .to_vec();
        let builder = builders.entry(name).or_default();
        let slot = match entry.name() {
            "path" => &mut builder.path,
            "url" => &mut builder.url,
            "branch" => &mut builder.branch,
            "update" => &mut builder.update,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(Error::InvalidRepository("duplicate submodule key".into()));
        }
        if builders.len() > options.max_modules {
            return Err(Error::InvalidRepository(
                "submodule count exceeds limit".into(),
            ));
        }
    }
    let selected = options.paths.iter().collect::<BTreeSet<_>>();
    let mut paths = BTreeSet::new();
    let mut output = Vec::new();
    for (name, builder) in builders {
        let path = builder.path.ok_or_else(|| {
            Error::InvalidRepository(format!(
                "submodule `{}` has no path",
                String::from_utf8_lossy(&name)
            ))
        })?;
        validate_submodule_path(&path)?;
        let url = builder.url.ok_or_else(|| {
            Error::InvalidRepository(format!(
                "submodule `{}` has no URL",
                String::from_utf8_lossy(&name)
            ))
        })?;
        if !paths.insert(path.clone()) {
            return Err(Error::InvalidRepository("duplicate submodule path".into()));
        }
        if selected.is_empty() || selected.contains(&path) {
            output.push(Submodule {
                name,
                path,
                url,
                branch: builder.branch,
                update: builder.update,
            });
        }
    }
    if !selected.is_empty() && output.len() != selected.len() {
        return Err(Error::NotFound(PathBuf::from(
            "unknown submodule selection",
        )));
    }
    output.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(output)
}

fn validate_submodule_path(path: &[u8]) -> Result<()> {
    if path.is_empty()
        || path.contains(&0)
        || path.starts_with(b"/")
        || path.ends_with(b"/")
        || path.split(|byte| *byte == b'/').any(|part| {
            part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git")
        })
    {
        return Err(Error::InvalidRepository("unsafe submodule path".into()));
    }
    Ok(())
}

fn subsection_value<'a>(config: &'a Config, subsection: &[u8], name: &str) -> Option<&'a [u8]> {
    config_value_in_subsection(config, "submodule", subsection, name)
}

fn config_value_in_subsection<'a>(
    config: &'a Config,
    section: &str,
    subsection: &[u8],
    name: &str,
) -> Option<&'a [u8]> {
    config
        .entries()
        .iter()
        .rev()
        .find(|entry| {
            entry.section() == section
                && entry.subsection() == Some(subsection)
                && entry.name() == name
        })
        .and_then(crate::ConfigEntry::value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CommitBuilder, EntryMode, FileSystem, Index, IndexEntry, IndexVersion, InitOptions,
        MemoryFileSystem, ObjectKind, PreviousValue, ReferenceName, RepositoryTransport, Signature,
        StatData, Tree, TreeEntry, UploadPackOptions,
    };

    const MODULES: &[u8] = b"[submodule \"lib\"]\n\tpath = deps/lib\n\turl = memory://lib\n\tbranch = stable\n\tupdate = checkout\n";

    fn fixture() -> (Repository, MemoryFileSystem, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "super", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("super/.gitmodules"), MODULES)
            .unwrap();
        let gitlink = ObjectId::from_bytes([7; ObjectId::LENGTH]);
        repository
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![
                        IndexEntry::new("deps/lib", 0o160_000, gitlink, StatData::default())
                            .unwrap(),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        (repository, filesystem, gitlink)
    }

    fn remote_fixture(filesystem: &MemoryFileSystem) -> (Repository, ObjectId) {
        let remote =
            Repository::init(filesystem.clone(), "remote-add", &InitOptions::default()).unwrap();
        let blob = remote.write_object(ObjectKind::Blob, b"nested\n").unwrap();
        let tree = remote
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file.txt".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("Sub", "sub@example.com", 1, 0).unwrap();
        let tip = remote
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .message(b"nested\n".to_vec())
                    .build(),
            )
            .unwrap();
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        (remote, tip)
    }

    fn assert_deinit_and_restore(
        superproject: &Repository,
        filesystem: &MemoryFileSystem,
        transport: &mut RepositoryTransport<'_>,
    ) {
        filesystem
            .write(Path::new("super-add/deps/lib/untracked"), b"local")
            .unwrap();
        assert!(
            superproject
                .deinit_submodule(b"deps/lib", &SubmoduleDeinitOptions::default())
                .is_err()
        );
        let deinitialized = superproject
            .deinit_submodule(
                b"deps/lib",
                &SubmoduleDeinitOptions {
                    force: true,
                    ..SubmoduleDeinitOptions::default()
                },
            )
            .unwrap();
        assert!(deinitialized.was_populated);
        assert!(
            filesystem
                .read_dir(Path::new("super-add/deps/lib"))
                .unwrap()
                .is_empty()
        );
        assert!(
            filesystem
                .exists(Path::new("super-add/.git/modules/library/HEAD"))
                .unwrap()
        );
        assert!(
            subsection_value(&superproject.read_config().unwrap(), b"library", "url").is_none()
        );
        let restored = superproject
            .update_submodule(
                b"deps/lib",
                transport,
                &SubmoduleUpdateOptions {
                    init: true,
                    ..SubmoduleUpdateOptions::default()
                },
            )
            .unwrap();
        assert!(restored.cloned);
        assert_eq!(
            filesystem
                .read(Path::new("super-add/deps/lib/file.txt"))
                .unwrap(),
            b"nested\n"
        );
    }

    #[test]
    fn parses_selects_and_rejects_unsafe_or_duplicate_modules() {
        let modules = parse_modules(MODULES, &SubmoduleOptions::default()).unwrap();
        assert_eq!(modules[0].path(), b"deps/lib");
        assert_eq!(modules[0].branch(), Some(b"stable".as_slice()));

        let unsafe_config = b"[submodule \"bad\"]\npath = ../escape\nurl = x\n";
        assert!(parse_modules(unsafe_config, &SubmoduleOptions::default()).is_err());
        let duplicate =
            b"[submodule \"a\"]\npath = x\nurl = a\n[submodule \"b\"]\npath = x\nurl = b\n";
        assert!(parse_modules(duplicate, &SubmoduleOptions::default()).is_err());
    }

    #[test]
    fn relative_urls_match_git_source_vectors() {
        for (base, relative, up, expected) in [
            ("../foo/bar", "../submodule", None, "../foo/submodule"),
            ("./foo/bar", "../submodule", None, "foo/submodule"),
            (
                "file:///tmp/repo",
                "../subrepo",
                None,
                "file:///tmp/subrepo",
            ),
            (
                "user@host:path/to/repo",
                "../subrepo",
                None,
                "user@host:path/to/subrepo",
            ),
            (
                "../foo/bar",
                "../sub/a/b/c",
                Some("../../../"),
                "../../../../foo/sub/a/b/c",
            ),
        ] {
            assert_eq!(
                resolve_relative_url(base.as_bytes(), relative.as_bytes(), up.map(str::as_bytes))
                    .unwrap(),
                expected.as_bytes()
            );
        }
    }

    #[test]
    fn status_reports_cached_and_uninitialized_gitlinks() {
        let (repository, _filesystem, gitlink) = fixture();
        let status = repository
            .submodule_status(&SubmoduleOptions::default())
            .unwrap();
        assert_eq!(status[0].kind(), SubmoduleStatusKind::Uninitialized);
        assert_eq!(status[0].expected(), Some(gitlink));

        let cached = repository
            .submodule_status(&SubmoduleOptions {
                cached: true,
                ..SubmoduleOptions::default()
            })
            .unwrap();
        assert_eq!(cached[0].kind(), SubmoduleStatusKind::Matches);
        assert_eq!(cached[0].actual(), Some(gitlink));
    }

    #[test]
    fn init_copies_safe_values_and_preserves_local_overrides() {
        let (repository, _filesystem, _) = fixture();
        repository
            .init_submodules(&SubmoduleOptions::default())
            .unwrap();
        let config = repository.read_config().unwrap();
        assert_eq!(
            subsection_value(&config, b"lib", "url"),
            Some(b"memory://lib".as_slice())
        );
        assert_eq!(
            subsection_value(&config, b"lib", "update"),
            Some(b"checkout".as_slice())
        );

        let mut config = config;
        config
            .set_in_subsection("submodule", b"lib", "url", b"local://override")
            .unwrap();
        repository.write_config(&config).unwrap();
        repository
            .init_submodules(&SubmoduleOptions::default())
            .unwrap();
        let config = repository.read_config().unwrap();
        assert_eq!(
            subsection_value(&config, b"lib", "url"),
            Some(b"local://override".as_slice())
        );
    }

    #[test]
    fn status_reports_gitlink_conflicts() {
        let (repository, _filesystem, gitlink) = fixture();
        let entries = [1, 2, 3]
            .into_iter()
            .map(|stage| {
                IndexEntry::with_stage("deps/lib", 0o160_000, gitlink, StatData::default(), stage)
                    .unwrap()
            })
            .collect();
        repository
            .write_index(&Index::new(IndexVersion::V2, entries).unwrap())
            .unwrap();
        let status = repository
            .submodule_status(&SubmoduleOptions::default())
            .unwrap();
        assert_eq!(status[0].kind(), SubmoduleStatusKind::Conflict);
        assert_eq!(status[0].prefix(), 'U');
    }

    #[test]
    fn deinit_absorbs_legacy_embedded_git_directory() {
        let (repository, filesystem, _) = fixture();
        Repository::init(
            filesystem.clone(),
            "super/deps/lib",
            &InitOptions::default(),
        )
        .unwrap();
        let report = repository
            .deinit_submodule(
                b"deps/lib",
                &SubmoduleDeinitOptions {
                    force: true,
                    ..SubmoduleDeinitOptions::default()
                },
            )
            .unwrap();
        assert!(report.was_populated);
        assert!(
            filesystem
                .exists(Path::new("super/.git/modules/lib/HEAD"))
                .unwrap()
        );
        assert!(
            filesystem
                .read_dir(Path::new("super/deps/lib"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn update_clones_through_transport_and_detaches_at_gitlink() {
        let filesystem = MemoryFileSystem::new();
        let remote =
            Repository::init(filesystem.clone(), "remote", &InitOptions::default()).unwrap();
        let blob = remote.write_object(ObjectKind::Blob, b"nested\n").unwrap();
        let tree = remote
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file.txt".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("Sub", "sub@example.com", 1, 0).unwrap();
        let tip = remote
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .message(b"nested\n".to_vec())
                    .build(),
            )
            .unwrap();
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();

        let superproject =
            Repository::init(filesystem.clone(), "super", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("super/.gitmodules"), MODULES)
            .unwrap();
        superproject
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![IndexEntry::new("deps/lib", 0o160_000, tip, StatData::default()).unwrap()],
                )
                .unwrap(),
            )
            .unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let report = superproject
            .update_submodule(
                b"deps/lib",
                &mut transport,
                &SubmoduleUpdateOptions {
                    init: true,
                    ..SubmoduleUpdateOptions::default()
                },
            )
            .unwrap();

        assert!(report.cloned);
        assert_eq!(report.current, tip);
        assert_eq!(
            filesystem
                .read(Path::new("super/deps/lib/file.txt"))
                .unwrap(),
            b"nested\n"
        );
        let nested = Repository::open(filesystem, "super/deps/lib").unwrap();
        assert_eq!(nested.resolve_reference("HEAD").unwrap(), tip);
        assert_eq!(
            superproject
                .submodule_status(&SubmoduleOptions::default())
                .unwrap()[0]
                .kind(),
            SubmoduleStatusKind::Matches
        );
    }

    #[test]
    fn add_clones_absorbed_layout_and_stages_metadata_and_gitlink() {
        let filesystem = MemoryFileSystem::new();
        let (remote, tip) = remote_fixture(&filesystem);
        let superproject =
            Repository::init(filesystem.clone(), "super-add", &InitOptions::default()).unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let report = superproject
            .add_submodule(
                b"deps/lib",
                b"memory://remote",
                &mut transport,
                &SubmoduleAddOptions {
                    name: Some(b"library".to_vec()),
                    branch: Some(b"main".to_vec()),
                    ..SubmoduleAddOptions::default()
                },
            )
            .unwrap();

        assert_eq!(report.gitlink, tip);
        assert_eq!(report.module.name(), b"library");
        assert_eq!(
            filesystem
                .read(Path::new("super-add/deps/lib/.git"))
                .unwrap(),
            b"gitdir: ../../.git/modules/library\n"
        );
        assert!(
            filesystem
                .exists(Path::new("super-add/.git/modules/library/HEAD"))
                .unwrap()
        );
        assert_eq!(
            filesystem
                .read(Path::new("super-add/deps/lib/file.txt"))
                .unwrap(),
            b"nested\n"
        );
        let nested = Repository::open(filesystem.clone(), "super-add/deps/lib").unwrap();
        assert_eq!(
            nested.read_git_file("HEAD").unwrap(),
            b"ref: refs/heads/main\n"
        );
        let index = superproject.read_index().unwrap();
        assert!(index.entries().iter().any(|entry| {
            entry.path() == b"deps/lib" && entry.mode() == 0o160_000 && entry.id() == tip
        }));
        assert!(
            index
                .entries()
                .iter()
                .any(|entry| entry.path() == b".gitmodules" && entry.mode() == 0o100_644)
        );
        let modules = superproject
            .submodules(&SubmoduleOptions::default())
            .unwrap();
        assert_eq!(modules, vec![report.module]);
        let mut super_config = superproject.read_config().unwrap();
        super_config
            .set_in_subsection("remote", b"origin", "url", b"../upstream/super")
            .unwrap();
        superproject.write_config(&super_config).unwrap();
        let mut modules_config =
            Config::parse(&filesystem.read(Path::new("super-add/.gitmodules")).unwrap()).unwrap();
        modules_config
            .set_in_subsection("submodule", b"library", "url", b"../changed")
            .unwrap();
        filesystem
            .write(Path::new("super-add/.gitmodules"), &modules_config.encode())
            .unwrap();
        assert_eq!(
            superproject
                .sync_submodules(&SubmoduleOptions::default())
                .unwrap(),
            vec![b"deps/lib".to_vec()]
        );
        assert_eq!(
            subsection_value(&superproject.read_config().unwrap(), b"library", "url"),
            Some(b"../upstream/changed".as_slice())
        );
        assert_eq!(
            config_value_in_subsection(&nested.read_config().unwrap(), "remote", b"origin", "url"),
            Some(b"../../../upstream/changed".as_slice())
        );
        assert!(
            superproject
                .add_submodule(
                    b"other",
                    b"memory://remote",
                    &mut transport,
                    &SubmoduleAddOptions {
                        name: Some(b"library/child".to_vec()),
                        ..SubmoduleAddOptions::default()
                    },
                )
                .is_err()
        );

        assert_deinit_and_restore(&superproject, &filesystem, &mut transport);
    }

    #[test]
    fn refuses_adding_submodule_through_a_symlinked_directory() {
        let filesystem = MemoryFileSystem::new();
        let (remote, _) = remote_fixture(&filesystem);
        let superproject =
            Repository::init(filesystem.clone(), "super", &InitOptions::default()).unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        filesystem
            .create_symlink(Path::new("super/link"), b"outside")
            .unwrap();
        assert!(matches!(
            superproject.add_submodule(
                b"link/nested",
                b"memory://remote",
                &mut transport,
                &SubmoduleAddOptions::default(),
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(!filesystem.exists(Path::new("super/link/nested")).unwrap());
    }

    #[test]
    fn refuses_deinitializing_submodule_through_a_symlinked_directory() {
        let filesystem = MemoryFileSystem::new();
        let superproject =
            Repository::init(filesystem.clone(), "super", &InitOptions::default()).unwrap();
        filesystem
            .write(
                Path::new("super/.gitmodules"),
                b"[submodule \"lib\"]\n\tpath = link/lib\n\turl = memory://lib\n",
            )
            .unwrap();
        filesystem
            .create_symlink(Path::new("super/link"), b"outside")
            .unwrap();
        assert!(matches!(
            superproject.deinit_submodule(b"link/lib", &SubmoduleDeinitOptions::default()),
            Err(Error::BeyondSymbolicLink(_))
        ));
    }

    #[test]
    fn refuses_updating_submodule_through_a_symlinked_directory() {
        let filesystem = MemoryFileSystem::new();
        let remote =
            Repository::init(filesystem.clone(), "remote", &InitOptions::default()).unwrap();
        let superproject =
            Repository::init(filesystem.clone(), "super", &InitOptions::default()).unwrap();
        filesystem
            .write(
                Path::new("super/.gitmodules"),
                b"[submodule \"lib\"]\n\tpath = link/lib\n\turl = memory://lib\n",
            )
            .unwrap();
        let tip = ObjectId::from_bytes([7; ObjectId::LENGTH]);
        superproject
            .write_index(
                &Index::new(
                    IndexVersion::V2,
                    vec![IndexEntry::new(
                        "link/lib",
                        0o160_000,
                        tip,
                        StatData::default(),
                    )
                    .unwrap()],
                )
                .unwrap(),
            )
            .unwrap();
        filesystem
            .create_symlink(Path::new("super/link"), b"outside")
            .unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        assert!(matches!(
            superproject.update_submodule(
                b"link/lib",
                &mut transport,
                &SubmoduleUpdateOptions {
                    init: true,
                    ..SubmoduleUpdateOptions::default()
                },
            ),
            Err(Error::BeyondSymbolicLink(_))
        ));
        assert!(!filesystem.exists(Path::new("super/link/lib")).unwrap());
    }
}
