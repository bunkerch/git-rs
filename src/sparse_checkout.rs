//! Sparse worktree selection using Git's skip-worktree index bit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::worktree::worktree_path;
use crate::{
    Config, Error, IgnoreMatcher, Index, IndexVersion, Repository, RestoreOptions, Result,
};

const PATTERN_PATH: &str = "info/sparse-checkout";
const WORKTREE_CONFIG: &str = "config.worktree";

/// Sparse selection mode and resource/collision policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseCheckoutOptions {
    pub cone: bool,
    /// Remove an excluded tracked path even when its worktree content differs.
    pub force: bool,
    pub dry_run: bool,
    pub max_patterns: usize,
    pub max_pattern_bytes: usize,
    pub max_index_entries: usize,
    pub max_object_size: usize,
}

impl Default for SparseCheckoutOptions {
    fn default() -> Self {
        Self {
            cone: true,
            force: false,
            dry_run: false,
            max_patterns: 1_000_000,
            max_pattern_bytes: 64 * 1024 * 1024,
            max_index_entries: 100_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

/// Persisted sparse-checkout mode and user-facing rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseCheckoutState {
    pub cone: bool,
    rules: Vec<Vec<u8>>,
}

impl SparseCheckoutState {
    #[must_use]
    pub fn rules(&self) -> &[Vec<u8>] {
        &self.rules
    }
}

/// Paths affected by applying a sparse definition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SparseCheckoutReport {
    pub included: usize,
    pub excluded: usize,
    pub materialized: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
    /// Modified, conflicted, obstructed, or gitlink paths kept in the worktree.
    pub retained: Vec<Vec<u8>>,
}

impl Repository {
    /// Replace the sparse definition and update the index/worktree.
    ///
    /// Cone rules are literal repository-relative directories. Non-cone rules
    /// use the same ordered byte-pattern grammar as `.gitignore`, but positive
    /// matches select paths for inclusion.
    ///
    /// # Errors
    /// Returns an error for a bare repository, unsafe/invalid rules, exceeded
    /// bounds, untracked obstructions, corrupt objects/index/config, lock
    /// contention, or storage failures.
    pub fn set_sparse_checkout(
        &self,
        rules: &[Vec<u8>],
        options: &SparseCheckoutOptions,
    ) -> Result<SparseCheckoutReport> {
        let normalized = validate_rules(rules, options)?;
        self.apply_sparse_checkout(&normalized, options)
    }

    /// Add rules to the active sparse definition and reapply it.
    ///
    /// # Errors
    /// Returns an error if sparse checkout is disabled, modes disagree, limits
    /// are exceeded, or applying the expanded definition fails.
    pub fn add_sparse_checkout(
        &self,
        rules: &[Vec<u8>],
        options: &SparseCheckoutOptions,
    ) -> Result<SparseCheckoutReport> {
        let current =
            self.sparse_checkout_state(options.max_patterns, options.max_pattern_bytes)?;
        if current.cone != options.cone {
            return Err(Error::InvalidRepository(
                "sparse-checkout mode differs from active definition".into(),
            ));
        }
        let mut combined = current.rules;
        combined.extend_from_slice(rules);
        let normalized = validate_rules(&combined, options)?;
        self.apply_sparse_checkout(&normalized, options)
    }

    /// Reapply the persisted sparse definition to index and worktree state.
    ///
    /// # Errors
    /// Returns an error if sparse checkout is disabled or state/application
    /// validation fails.
    pub fn reapply_sparse_checkout(
        &self,
        options: &SparseCheckoutOptions,
    ) -> Result<SparseCheckoutReport> {
        let state = self.sparse_checkout_state(options.max_patterns, options.max_pattern_bytes)?;
        if state.cone != options.cone {
            return Err(Error::InvalidRepository(
                "sparse-checkout mode differs from active definition".into(),
            ));
        }
        self.apply_sparse_checkout(&state.rules, options)
    }

    /// Read the active sparse definition.
    ///
    /// # Errors
    /// Returns an error when sparse checkout is disabled, config/pattern data
    /// is malformed, or caller limits are exceeded.
    pub fn sparse_checkout_state(
        &self,
        max_patterns: usize,
        max_pattern_bytes: usize,
    ) -> Result<SparseCheckoutState> {
        let config = self.read_worktree_config()?;
        if config.get("core.sparseCheckout")?.is_none()
            || !config.get_bool("core.sparseCheckout")?
        {
            return Err(Error::InvalidRepository(
                "sparse checkout is not enabled".into(),
            ));
        }
        let cone = config.get("core.sparseCheckoutCone")?.is_some()
            && config.get_bool("core.sparseCheckoutCone")?;
        let contents = self.filesystem().read(&self.git_dir().join(PATTERN_PATH))?;
        if contents.len() > max_pattern_bytes {
            return Err(Error::ObjectTooLarge {
                declared: contents.len() as u64,
                limit: max_pattern_bytes,
            });
        }
        let rules = if cone {
            decode_cone_patterns(&contents)?
        } else {
            split_patterns(&contents)
        };
        if rules.len() > max_patterns {
            return Err(Error::InvalidRepository(
                "sparse-checkout pattern count exceeds limit".into(),
            ));
        }
        Ok(SparseCheckoutState { cone, rules })
    }

    /// Disable sparsity and materialize every missing tracked path.
    ///
    /// Existing worktree modifications are preserved.
    ///
    /// # Errors
    /// Returns an error for disabled/bare repositories, untracked obstructions,
    /// corrupt objects/index/config, lock contention, or storage failures.
    pub fn disable_sparse_checkout(
        &self,
        options: &SparseCheckoutOptions,
    ) -> Result<SparseCheckoutReport> {
        self.sparse_checkout_state(options.max_patterns, options.max_pattern_bytes)?;
        let all = [b"**".to_vec()];
        let mut non_cone = options.clone();
        non_cone.cone = false;
        let report = self.apply_sparse_selection(&all, &non_cone, false)?;
        if !options.dry_run {
            let mut config = self.read_worktree_config()?;
            config.set("core.sparseCheckout", b"false")?;
            self.write_worktree_config(&config)?;
            let pattern = self.git_dir().join(PATTERN_PATH);
            match self.filesystem().remove_file(&pattern) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }

    fn apply_sparse_checkout(
        &self,
        rules: &[Vec<u8>],
        options: &SparseCheckoutOptions,
    ) -> Result<SparseCheckoutReport> {
        let report = self.apply_sparse_selection(rules, options, true)?;
        if options.dry_run {
            return Ok(report);
        }
        let patterns = if options.cone {
            encode_cone_patterns(rules)
        } else {
            encode_patterns(rules)
        };
        self.write_git_dir_atomic(Path::new(PATTERN_PATH), &patterns)?;
        let mut common = self.read_config()?;
        common.set("extensions.worktreeConfig", b"true")?;
        self.write_config(&common)?;
        let mut worktree = self.read_worktree_config()?;
        worktree.set("core.sparseCheckout", b"true")?;
        let cone_value: &[u8] = if options.cone { b"true" } else { b"false" };
        worktree.set("core.sparseCheckoutCone", cone_value)?;
        worktree.set("index.sparse", b"false")?;
        self.write_worktree_config(&worktree)?;
        Ok(report)
    }

    #[allow(clippy::too_many_lines)]
    fn apply_sparse_selection(
        &self,
        rules: &[Vec<u8>],
        options: &SparseCheckoutOptions,
        persist_skip_bits: bool,
    ) -> Result<SparseCheckoutReport> {
        let root = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("sparse checkout requires a worktree".into())
        })?;
        let index = self.read_index()?;
        if index.entries().len() > options.max_index_entries {
            return Err(Error::InvalidRepository(
                "sparse-checkout index entry count exceeds limit".into(),
            ));
        }
        let matcher = (!options.cone)
            .then(|| pattern_matcher(rules))
            .transpose()?;
        let mut entries = Vec::with_capacity(index.entries().len());
        let mut report = SparseCheckoutReport::default();
        let mut materialize = Vec::<PathBuf>::new();
        let mut remove = Vec::<(Vec<u8>, PathBuf)>::new();
        for entry in index.entries() {
            let included = entry.stage() != 0
                || if options.cone {
                    cone_includes(entry.path(), rules)
                } else {
                    matcher
                        .as_ref()
                        .is_some_and(|matcher| matcher.is_ignored(entry.path(), false))
                };
            if included {
                report.included += 1;
                let full = root.join(worktree_path(entry.path())?);
                match self.filesystem().metadata(&full) {
                    Ok(_) => {}
                    Err(Error::NotFound(_)) if entry.stage() == 0 => {
                        materialize.push(worktree_path(entry.path())?);
                        report.materialized.push(entry.path().to_vec());
                    }
                    Err(Error::NotFound(_)) => report.retained.push(entry.path().to_vec()),
                    Err(error) => return Err(error),
                }
                entries.push(entry.clone().with_skip_worktree(false));
                continue;
            }
            report.excluded += 1;
            let relative = worktree_path(entry.path())?;
            let full = root.join(&relative);
            let removable = match self.filesystem().metadata(&full) {
                Ok(metadata) if metadata.is_dir() => false,
                Ok(_) if options.force => true,
                Ok(_) => self.worktree_matches(entry, &full)?,
                Err(Error::NotFound(_)) => true,
                Err(error) => return Err(error),
            };
            if removable {
                if self.filesystem().exists(&full)? {
                    remove.push((entry.path().to_vec(), relative));
                    report.removed.push(entry.path().to_vec());
                }
                entries.push(entry.clone().with_skip_worktree(persist_skip_bits));
            } else {
                report.retained.push(entry.path().to_vec());
                entries.push(entry.clone().with_skip_worktree(false));
            }
        }
        if !materialize.is_empty() {
            self.restore_paths(
                &materialize,
                &RestoreOptions {
                    dry_run: true,
                    max_object_size: options.max_object_size,
                    ..RestoreOptions::default()
                },
            )?;
        }
        if options.dry_run {
            return Ok(report);
        }
        for (_, relative) in &remove {
            let full = root.join(relative);
            self.filesystem().remove_file(&full)?;
            self.prune_empty_parents(root, relative.parent())?;
        }
        if !materialize.is_empty() {
            self.restore_paths(
                &materialize,
                &RestoreOptions {
                    max_object_size: options.max_object_size,
                    ..RestoreOptions::default()
                },
            )?;
        }
        let version = if entries.iter().any(crate::IndexEntry::skip_worktree)
            && index.version() == IndexVersion::V2
        {
            IndexVersion::V3
        } else {
            index.version()
        };
        self.write_index(&Index::new(version, entries)?)?;
        Ok(report)
    }

    fn read_worktree_config(&self) -> Result<Config> {
        match self
            .filesystem()
            .read(&self.git_dir().join(WORKTREE_CONFIG))
        {
            Ok(data) => Config::parse(&data),
            Err(Error::NotFound(_)) => Ok(Config::default()),
            Err(error) => Err(error),
        }
    }

    fn write_worktree_config(&self, config: &Config) -> Result<()> {
        self.write_git_dir_atomic(Path::new(WORKTREE_CONFIG), &config.encode())
    }

    fn write_git_dir_atomic(&self, relative: &Path, contents: &[u8]) -> Result<()> {
        let destination = self.git_dir().join(relative);
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let mut lock_name = destination
            .file_name()
            .ok_or_else(|| Error::InvalidPath(destination.clone()))?
            .to_os_string();
        lock_name.push(".lock");
        let lock = destination.with_file_name(lock_name);
        self.filesystem().write_new(&lock, contents)?;
        if let Err(error) = self.filesystem().rename(&lock, &destination) {
            let _ = self.filesystem().remove_file(&lock);
            return Err(error);
        }
        Ok(())
    }
}

fn validate_rules(rules: &[Vec<u8>], options: &SparseCheckoutOptions) -> Result<Vec<Vec<u8>>> {
    if rules.len() > options.max_patterns {
        return Err(Error::InvalidRepository(
            "sparse-checkout pattern count exceeds limit".into(),
        ));
    }
    let bytes = rules.iter().try_fold(0usize, |total, rule| {
        total
            .checked_add(rule.len())
            .ok_or_else(|| Error::InvalidRepository("sparse-checkout pattern size overflow".into()))
    })?;
    if bytes > options.max_pattern_bytes {
        return Err(Error::ObjectTooLarge {
            declared: bytes as u64,
            limit: options.max_pattern_bytes,
        });
    }
    let mut output = Vec::with_capacity(rules.len());
    for rule in rules {
        if rule.is_empty() || rule.contains(&0) || rule.contains(&b'\n') || rule.contains(&b'\r') {
            return Err(Error::InvalidRepository(
                "invalid sparse-checkout rule".into(),
            ));
        }
        let normalized = if options.cone {
            normalize_cone(rule)?
        } else {
            rule.clone()
        };
        output.push(normalized);
    }
    if options.cone {
        output.sort_unstable();
        output.dedup();
        let candidates = output.clone();
        output.retain(|rule| {
            !candidates.iter().any(|other| {
                other.len() < rule.len()
                    && rule.starts_with(other)
                    && rule.get(other.len()) == Some(&b'/')
            })
        });
    }
    Ok(output)
}

fn normalize_cone(rule: &[u8]) -> Result<Vec<u8>> {
    let rule = rule.strip_suffix(b"/").unwrap_or(rule);
    if rule.is_empty()
        || rule.starts_with(b"/")
        || rule
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(Error::InvalidRepository(
            "invalid sparse-checkout cone directory".into(),
        ));
    }
    Ok(rule.to_vec())
}

fn cone_includes(path: &[u8], cones: &[Vec<u8>]) -> bool {
    let parent = path
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(b"".as_slice(), |position| &path[..position]);
    parent.is_empty()
        || cones.iter().any(|cone| {
            (path.starts_with(cone) && path.get(cone.len()) == Some(&b'/'))
                || (cone.starts_with(parent)
                    && (parent.is_empty() || cone.get(parent.len()) == Some(&b'/')))
        })
}

fn pattern_matcher(rules: &[Vec<u8>]) -> Result<IgnoreMatcher> {
    let mut matcher = IgnoreMatcher::default();
    matcher.add_patterns(b"", &encode_patterns(rules))?;
    Ok(matcher)
}

fn encode_patterns(rules: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    for rule in rules {
        output.extend_from_slice(rule);
        output.push(b'\n');
    }
    output
}

fn encode_cone_patterns(cones: &[Vec<u8>]) -> Vec<u8> {
    let mut directories = BTreeSet::new();
    for cone in cones {
        let mut current = Vec::new();
        for component in cone.split(|byte| *byte == b'/') {
            if !current.is_empty() {
                current.push(b'/');
            }
            current.extend_from_slice(component);
            directories.insert(current.clone());
        }
    }
    let selected = cones.iter().cloned().collect::<BTreeSet<_>>();
    let mut output = b"/*\n!/*/\n".to_vec();
    for directory in directories {
        output.push(b'/');
        append_escaped_pattern(&mut output, &directory);
        output.extend_from_slice(b"/\n");
        if !selected.contains(&directory) {
            output.extend_from_slice(b"!/");
            append_escaped_pattern(&mut output, &directory);
            output.extend_from_slice(b"/*/\n");
        }
    }
    output
}

fn decode_cone_patterns(contents: &[u8]) -> Result<Vec<Vec<u8>>> {
    let lines = split_patterns(contents);
    if lines.first().map(Vec::as_slice) != Some(b"/*")
        || lines.get(1).map(Vec::as_slice) != Some(b"!/*/")
    {
        return Err(Error::InvalidRepository(
            "invalid cone sparse-checkout header".into(),
        ));
    }
    let positive = lines
        .iter()
        .skip(2)
        .filter_map(|line| {
            line.strip_prefix(b"/")
                .and_then(|line| line.strip_suffix(b"/"))
                .map(unescape_pattern)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(positive
        .iter()
        .filter(|candidate| {
            !positive.iter().any(|other| {
                other.len() > candidate.len()
                    && other.starts_with(candidate.as_slice())
                    && other.get(candidate.len()) == Some(&b'/')
            })
        })
        .cloned()
        .collect())
}

fn append_escaped_pattern(output: &mut Vec<u8>, value: &[u8]) {
    for byte in value {
        if matches!(byte, b'\\' | b'*' | b'?' | b'[') {
            output.push(b'\\');
        }
        output.push(*byte);
    }
}

fn unescape_pattern(value: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\' {
            index += 1;
            output.push(*value.get(index).ok_or_else(|| {
                Error::InvalidRepository("trailing escape in cone pattern".into())
            })?);
        } else {
            output.push(value[index]);
        }
        index += 1;
    }
    Ok(output)
}

fn split_patterns(contents: &[u8]) -> Vec<Vec<u8>> {
    contents
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        CommitOptions, FileSystem, InitOptions, MemoryFileSystem, Repository, Signature,
        SparseCheckoutOptions,
    };

    fn repository() -> (Repository, MemoryFileSystem) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        for (path, data) in [
            ("root.txt", b"root".as_slice()),
            ("a/file.txt", b"ancestor"),
            ("a/b/deep.txt", b"selected"),
            ("a/c/drop.txt", b"drop-a"),
            ("z/drop.txt", b"drop-z"),
        ] {
            let full = Path::new("repo").join(path);
            filesystem.create_dir_all(full.parent().unwrap()).unwrap();
            filesystem.write(&full, data).unwrap();
        }
        repository.add(".").unwrap();
        let signature = Signature::new("S", "s@example.com", 1, 0).unwrap();
        repository
            .commit_index(b"tree", &signature, &signature, &CommitOptions::default())
            .unwrap();
        (repository, filesystem)
    }

    #[test]
    fn cone_mode_keeps_root_ancestor_siblings_and_selected_descendants() {
        let (repository, filesystem) = repository();
        let report = repository
            .set_sparse_checkout(&[b"a/b".to_vec()], &SparseCheckoutOptions::default())
            .unwrap();
        assert_eq!(
            (report.included, report.excluded, report.removed.len()),
            (3, 2, 2)
        );
        for path in ["root.txt", "a/file.txt", "a/b/deep.txt"] {
            assert!(filesystem.exists(&Path::new("repo").join(path)).unwrap());
        }
        for path in ["a/c/drop.txt", "z/drop.txt"] {
            assert!(!filesystem.exists(&Path::new("repo").join(path)).unwrap());
        }
        let index = repository.read_index().unwrap();
        assert_eq!(index.version(), crate::IndexVersion::V3);
        assert_eq!(
            index
                .entries()
                .iter()
                .filter(|entry| entry.skip_worktree())
                .count(),
            2
        );
        assert_eq!(
            repository.sparse_checkout_state(10, 1024).unwrap().rules(),
            &[b"a/b".to_vec()]
        );
    }

    #[test]
    fn modified_exclusion_is_retained_and_disable_restores_missing_paths() {
        let (repository, filesystem) = repository();
        filesystem
            .write(Path::new("repo/a/c/drop.txt"), b"local")
            .unwrap();
        let report = repository
            .set_sparse_checkout(&[b"a/b".to_vec()], &SparseCheckoutOptions::default())
            .unwrap();
        assert_eq!(report.retained, vec![b"a/c/drop.txt".to_vec()]);
        let index = repository.read_index().unwrap();
        let retained = index
            .entries()
            .iter()
            .find(|entry| entry.path() == b"a/c/drop.txt")
            .unwrap();
        assert!(!retained.skip_worktree());

        repository
            .disable_sparse_checkout(&SparseCheckoutOptions::default())
            .unwrap();
        assert_eq!(
            filesystem.read(Path::new("repo/a/c/drop.txt")).unwrap(),
            b"local"
        );
        assert_eq!(
            filesystem.read(Path::new("repo/z/drop.txt")).unwrap(),
            b"drop-z"
        );
        assert!(
            repository
                .read_index()
                .unwrap()
                .entries()
                .iter()
                .all(|entry| !entry.skip_worktree())
        );
        assert!(repository.sparse_checkout_state(10, 1024).is_err());
    }

    #[test]
    fn non_cone_ordered_patterns_and_dry_run_are_honored() {
        let (repository, filesystem) = repository();
        let rules = [b"/**".to_vec(), b"!/a/c/**".to_vec()];
        let preview = repository
            .set_sparse_checkout(
                &rules,
                &SparseCheckoutOptions {
                    cone: false,
                    dry_run: true,
                    ..SparseCheckoutOptions::default()
                },
            )
            .unwrap();
        assert_eq!(preview.excluded, 1);
        assert!(filesystem.exists(Path::new("repo/a/c/drop.txt")).unwrap());
        assert!(repository.sparse_checkout_state(10, 1024).is_err());

        repository
            .set_sparse_checkout(
                &rules,
                &SparseCheckoutOptions {
                    cone: false,
                    ..SparseCheckoutOptions::default()
                },
            )
            .unwrap();
        assert!(!filesystem.exists(Path::new("repo/a/c/drop.txt")).unwrap());
        assert!(filesystem.exists(Path::new("repo/z/drop.txt")).unwrap());
    }

    #[test]
    fn cone_pattern_encoding_round_trips_git_metacharacters() {
        let rules = vec![br"odd*?[\name".to_vec()];
        let encoded = super::encode_cone_patterns(&rules);
        assert_eq!(super::decode_cone_patterns(&encoded).unwrap(), rules);
    }

    #[test]
    fn add_materializes_new_cone_and_reapply_removes_clean_leaks() {
        let (repository, filesystem) = repository();
        repository
            .set_sparse_checkout(&[b"a/b".to_vec()], &SparseCheckoutOptions::default())
            .unwrap();
        let added = repository
            .add_sparse_checkout(&[b"z".to_vec()], &SparseCheckoutOptions::default())
            .unwrap();
        assert_eq!(added.materialized, vec![b"z/drop.txt".to_vec()]);
        assert_eq!(
            repository.sparse_checkout_state(10, 1024).unwrap().rules(),
            &[b"a/b".to_vec(), b"z".to_vec()]
        );
        filesystem.create_dir_all(Path::new("repo/a/c")).unwrap();
        filesystem
            .write(Path::new("repo/a/c/drop.txt"), b"drop-a")
            .unwrap();
        let reapplied = repository
            .reapply_sparse_checkout(&SparseCheckoutOptions::default())
            .unwrap();
        assert_eq!(reapplied.removed, vec![b"a/c/drop.txt".to_vec()]);
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/config.worktree.lock"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/config.lock"))
                .unwrap()
        );
    }
}
