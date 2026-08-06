//! Gitlink-backed submodule discovery, initialization, status, and update.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::worktree::worktree_path;
use crate::{
    CheckoutOptions, CloneOptions, Config, Error, FetchOptions, ObjectId, Repository, Result,
    UploadPackTransport,
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
        let nested_path = self
            .work_tree()
            .ok_or_else(|| Error::InvalidRepository("submodules require a worktree".into()))?
            .join(worktree_path(module.path())?);
        let (nested, cloned, fetched_objects) =
            self.obtain_submodule_repository(&nested_path, url, transport, options)?;
        nested.read_commit(expected, options.max_object_size)?;
        let previous = nested.resolve_reference("HEAD").ok();
        if previous != Some(expected) || options.force {
            let commit = nested.read_commit(expected, options.max_object_size)?;
            nested.checkout_tree(
                commit.tree(),
                &CheckoutOptions {
                    force: options.force,
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

    fn obtain_submodule_repository<T: UploadPackTransport>(
        &self,
        nested_path: &Path,
        url: &[u8],
        transport: &mut T,
        options: &SubmoduleUpdateOptions,
    ) -> Result<(Repository, bool, usize)> {
        if !self.filesystem().exists(&nested_path.join(".git"))? {
            return self.clone_submodule(nested_path, url, transport, options);
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
        url: &[u8],
        transport: &mut T,
        options: &SubmoduleUpdateOptions,
    ) -> Result<(Repository, bool, usize)> {
        let url = std::str::from_utf8(url)
            .map_err(|_| Error::InvalidRepository("submodule URL is not UTF-8".into()))?;
        let (repository, fetched) = Repository::clone_from_shared(
            self.shared_filesystem(),
            nested_path,
            transport,
            &CloneOptions {
                remote_url: url.to_owned(),
                checkout: true,
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
                ..CloneOptions::default()
            },
        )?;
        Ok((repository, true, fetched.received_objects))
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
    config
        .entries()
        .iter()
        .rev()
        .find(|entry| {
            entry.section() == "submodule"
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
}
