//! Atomic loose-reference consolidation into Git's `packed-refs` format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::ignore::wildmatch_ref;
use crate::{Error, ObjectId, ObjectKind, ReferenceTarget, Repository, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackRefsOptions {
    /// Select every packable ref. Otherwise tags are selected by default.
    pub all: bool,
    /// Remove selected loose refs after the packed file is published.
    pub prune: bool,
    /// Additional wildmatch patterns. When nonempty these replace tag-default selection.
    pub include: Vec<Vec<u8>>,
    pub exclude: Vec<Vec<u8>>,
    pub max_object_size: usize,
    pub max_tag_depth: usize,
    pub max_refs: usize,
    /// Validate and report selected loose refs without locks or publication.
    pub dry_run: bool,
}

impl Default for PackRefsOptions {
    fn default() -> Self {
        Self {
            all: false,
            prune: true,
            include: Vec::new(),
            exclude: Vec::new(),
            max_object_size: 1024 * 1024 * 1024,
            max_tag_depth: 64,
            max_refs: 10_000_000,
            dry_run: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackRefsResult {
    packed: Vec<String>,
    pub pruned: usize,
}

impl PackRefsResult {
    #[must_use]
    pub fn packed(&self) -> &[String] {
        &self.packed
    }
}

#[derive(Clone)]
struct PackedValue {
    id: ObjectId,
    peeled: Option<ObjectId>,
}

struct LooseRef {
    name: String,
    path: PathBuf,
    lock: PathBuf,
    id: ObjectId,
    kind: ObjectKind,
}

impl Repository {
    /// Atomically merge selected loose references into `packed-refs`.
    ///
    /// Symbolic refs are never packed. Existing packed-only entries and their
    /// peeled values are retained. Selected loose locks remain held through
    /// publication and optional pruning, preventing stale loose deletion.
    ///
    /// # Errors
    /// Returns an error for malformed patterns/refs, resource limits, missing
    /// selected objects, tag peeling, lock contention, or storage failures.
    pub fn pack_refs(&self, options: &PackRefsOptions) -> Result<PackRefsResult> {
        validate_patterns(&options.include)?;
        validate_patterns(&options.exclude)?;
        let mut loose = self.packable_loose_refs(options)?;
        if loose.is_empty() {
            return Ok(PackRefsResult {
                packed: Vec::new(),
                pruned: 0,
            });
        }
        loose.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        if options.dry_run {
            let existing = read_packed_values(self, &self.git_path("packed-refs"))?;
            if existing.len().saturating_add(loose.len()) > options.max_refs {
                return Err(Error::InvalidRepository(
                    "packed reference limit exceeded".into(),
                ));
            }
            for item in &loose {
                if item.kind == ObjectKind::Tag {
                    self.peel_tag(item.id, options.max_tag_depth, options.max_object_size)?;
                }
            }
            return Ok(PackRefsResult {
                packed: loose.into_iter().map(|item| item.name).collect(),
                pruned: 0,
            });
        }
        let mut locked = Vec::with_capacity(loose.len());
        for item in loose {
            if let Err(error) = self.filesystem().write_new(&item.lock, b"") {
                cleanup_loose_locks(self, &locked);
                return Err(error);
            }
            locked.push(item);
        }
        let packed_path = self.git_path("packed-refs");
        let packed_lock = self.git_path("packed-refs.lock");
        if let Err(error) = self.filesystem().write_new(&packed_lock, b"") {
            cleanup_loose_locks(self, &locked);
            return Err(error);
        }
        let result = self.pack_refs_locked(&locked, options, &packed_path, &packed_lock);
        if result.is_err() {
            cleanup_loose_locks(self, &locked);
            let _ = self.filesystem().remove_file(&packed_lock);
        }
        result
    }

    fn pack_refs_locked(
        &self,
        locked: &[LooseRef],
        options: &PackRefsOptions,
        packed_path: &Path,
        packed_lock: &Path,
    ) -> Result<PackRefsResult> {
        let mut packed = read_packed_values(self, packed_path)?;
        for item in locked {
            let reference = self.read_reference(&item.name)?;
            let id = match reference.target() {
                ReferenceTarget::Direct(id) if *id == item.id => *id,
                ReferenceTarget::Direct(_) | ReferenceTarget::Symbolic(_) => {
                    return Err(Error::ReferenceConflict(item.name.clone()));
                }
            };
            let peeled = if item.kind == ObjectKind::Tag {
                Some(
                    self.peel_tag(id, options.max_tag_depth, options.max_object_size)?
                        .id,
                )
            } else {
                None
            };
            packed.insert(item.name.clone(), PackedValue { id, peeled });
        }
        if packed.len() > options.max_refs {
            return Err(Error::InvalidRepository(
                "packed reference limit exceeded".into(),
            ));
        }
        self.filesystem()
            .write(packed_lock, &encode_packed_values(&packed))?;
        self.filesystem().rename(packed_lock, packed_path)?;

        let mut pruned = 0;
        for item in locked {
            if options.prune {
                self.filesystem().remove_file(&item.path)?;
                self.filesystem().remove_file(&item.lock)?;
                pruned += 1;
                prune_ref_parents(self, item.path.parent())?;
            } else {
                self.filesystem().remove_file(&item.lock)?;
            }
        }
        Ok(PackRefsResult {
            packed: locked.iter().map(|item| item.name.clone()).collect(),
            pruned,
        })
    }

    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn packable_loose_refs(&self, options: &PackRefsOptions) -> Result<Vec<LooseRef>> {
        let root = self.git_path("refs");
        let mut directories = vec![(root, String::from("refs"))];
        let mut output = Vec::new();
        while let Some((directory, prefix)) = directories.pop() {
            for child in self.filesystem().read_dir(&directory)? {
                let path = directory.join(&child);
                let name = format!("{prefix}/{}", child.to_string_lossy());
                let metadata = self.filesystem().metadata(&path)?;
                if metadata.is_dir() {
                    directories.push((path, name));
                    continue;
                }
                if !metadata.is_file()
                    || name.ends_with(".lock")
                    || is_per_worktree_ref(&name)
                    || !selected_ref(&name, options)
                {
                    continue;
                }
                let reference = self.read_reference(&name)?;
                let ReferenceTarget::Direct(id) = reference.target() else {
                    continue;
                };
                let object = match self.read_object(*id, options.max_object_size) {
                    Ok(object) => object,
                    Err(Error::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                output.push(LooseRef {
                    name,
                    lock: append_lock(&path),
                    path,
                    id: *id,
                    kind: object.kind(),
                });
                if output.len() > options.max_refs {
                    return Err(Error::InvalidRepository(
                        "loose reference limit exceeded".into(),
                    ));
                }
            }
        }
        Ok(output)
    }
}

fn selected_ref(name: &str, options: &PackRefsOptions) -> bool {
    let bytes = name.as_bytes();
    let included = if options.all {
        true
    } else if options.include.is_empty() {
        name.starts_with("refs/tags/")
    } else {
        options
            .include
            .iter()
            .any(|pattern| wildmatch_ref(pattern, bytes))
    };
    included
        && !options
            .exclude
            .iter()
            .any(|pattern| wildmatch_ref(pattern, bytes))
}

fn is_per_worktree_ref(name: &str) -> bool {
    ["refs/bisect/", "refs/worktree/", "refs/rewritten/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn validate_patterns(patterns: &[Vec<u8>]) -> Result<()> {
    if patterns
        .iter()
        .any(|pattern| pattern.is_empty() || pattern.contains(&0))
    {
        return Err(Error::InvalidReference(
            "empty or NUL-containing pack-refs pattern".into(),
        ));
    }
    Ok(())
}

fn read_packed_values(
    repository: &Repository,
    path: &Path,
) -> Result<BTreeMap<String, PackedValue>> {
    let contents = match repository.filesystem().read(path) {
        Ok(contents) => contents,
        Err(Error::NotFound(_)) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let mut values = BTreeMap::<String, PackedValue>::new();
    let mut previous = None::<String>;
    for line in contents.split(|byte| *byte == b'\n') {
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        if let Some(hex) = line.strip_prefix(b"^") {
            let name = previous
                .as_ref()
                .ok_or_else(|| Error::InvalidReference("orphan packed peeled line".into()))?;
            values
                .get_mut(name)
                .expect("previous packed ref exists")
                .peeled = Some(parse_id(hex)?);
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::InvalidReference("malformed packed-refs entry".into()))?;
        let name = std::str::from_utf8(&line[separator + 1..])
            .map_err(|_| Error::InvalidReference("non-UTF-8 packed ref name".into()))?
            .to_owned();
        crate::ReferenceName::new(name.clone())?;
        if values
            .insert(
                name.clone(),
                PackedValue {
                    id: parse_id(&line[..separator])?,
                    peeled: None,
                },
            )
            .is_some()
        {
            return Err(Error::InvalidReference("duplicate packed reference".into()));
        }
        previous = Some(name);
    }
    Ok(values)
}

fn parse_id(value: &[u8]) -> Result<ObjectId> {
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidReference("non-ASCII packed object ID".into()))?;
    ObjectId::from_str(value)
}

fn encode_packed_values(values: &BTreeMap<String, PackedValue>) -> Vec<u8> {
    let mut output = b"# pack-refs with: peeled fully-peeled sorted \n".to_vec();
    for (name, value) in values {
        output.extend_from_slice(value.id.to_string().as_bytes());
        output.push(b' ');
        output.extend_from_slice(name.as_bytes());
        output.push(b'\n');
        if let Some(peeled) = value.peeled {
            output.push(b'^');
            output.extend_from_slice(peeled.to_string().as_bytes());
            output.push(b'\n');
        }
    }
    output
}

fn cleanup_loose_locks(repository: &Repository, refs: &[LooseRef]) {
    for item in refs {
        let _ = repository.filesystem().remove_file(&item.lock);
    }
}

fn append_lock(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    value.into()
}

fn prune_ref_parents(repository: &Repository, mut parent: Option<&Path>) -> Result<()> {
    let root = repository.git_path("refs");
    while let Some(path) = parent {
        if path == root {
            break;
        }
        match repository.filesystem().remove_dir(path) {
            Ok(()) | Err(Error::DirectoryNotEmpty(_) | Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        parent = path.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, FileSystem, InitOptions, MemoryFileSystem, Signature, TagBuilder};

    fn fixture() -> (Repository, MemoryFileSystem, ObjectId, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/file"), b"content")
            .unwrap();
        repository.add("file").unwrap();
        let signature = Signature::new("Refs", "refs@example.com", 100, 0).unwrap();
        let commit = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        repository
            .create_branch("topic/nested", commit, false)
            .unwrap();
        repository
            .create_lightweight_tag("light", commit, false, 1024)
            .unwrap();
        let tag = TagBuilder::new(commit, ObjectKind::Commit, b"annotated", signature)
            .unwrap()
            .message(b"tag".to_vec())
            .build();
        let (_, tag_id) = repository
            .create_annotated_tag("annotated", &tag, false, 1024)
            .unwrap();
        (repository, filesystem, commit, tag_id)
    }

    #[test]
    fn default_packs_tags_with_peeled_lines_and_prunes_only_tags() {
        let (repository, filesystem, commit, tag_id) = fixture();
        let result = repository.pack_refs(&PackRefsOptions::default()).unwrap();
        assert_eq!(result.packed(), &["refs/tags/annotated", "refs/tags/light"]);
        assert_eq!(result.pruned, 2);
        let packed = filesystem.read(Path::new("repo/.git/packed-refs")).unwrap();
        let text = String::from_utf8(packed).unwrap();
        assert!(text.contains(&format!("{tag_id} refs/tags/annotated\n^{commit}\n")));
        assert!(text.contains(&format!("{commit} refs/tags/light\n")));
        assert!(!text.contains("refs/heads/main"));
        assert!(
            filesystem
                .exists(Path::new("repo/.git/refs/heads/main"))
                .unwrap()
        );
        assert!(!filesystem.exists(Path::new("repo/.git/refs/tags")).unwrap());
        assert_eq!(
            repository.resolve_reference("refs/tags/annotated").unwrap(),
            tag_id
        );
    }

    #[test]
    fn include_star_crosses_slashes_and_exclusions_win() {
        let (repository, filesystem, _, _) = fixture();
        let result = repository
            .pack_refs(&PackRefsOptions {
                include: vec![b"refs/heads/*".to_vec()],
                exclude: vec![b"refs/heads/main".to_vec()],
                ..PackRefsOptions::default()
            })
            .unwrap();
        assert_eq!(result.packed(), &["refs/heads/topic/nested"]);
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/refs/heads/topic/nested"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/.git/refs/heads/main"))
                .unwrap()
        );
    }

    #[test]
    fn no_prune_keeps_loose_refs_and_symbolic_refs_are_skipped() {
        let (repository, filesystem, _, _) = fixture();
        filesystem
            .write(
                Path::new("repo/.git/refs/heads/symbolic"),
                b"ref: refs/heads/main\n",
            )
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/.git/refs/bisect"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/.git/refs/bisect/local"),
                format!("{}\n", repository.resolve_reference("HEAD").unwrap()).as_bytes(),
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/.git/refs/heads/broken"),
                b"1111111111111111111111111111111111111111\n",
            )
            .unwrap();
        let result = repository
            .pack_refs(&PackRefsOptions {
                all: true,
                prune: false,
                ..PackRefsOptions::default()
            })
            .unwrap();
        assert!(
            !result
                .packed()
                .iter()
                .any(|name| name.ends_with("symbolic"))
        );
        assert!(!result.packed().iter().any(|name| name.contains("bisect")));
        assert!(!result.packed().iter().any(|name| name.ends_with("broken")));
        assert!(
            filesystem
                .exists(Path::new("repo/.git/refs/tags/light"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/.git/refs/heads/main"))
                .unwrap()
        );
        assert_eq!(result.pruned, 0);
    }

    #[test]
    fn preserves_existing_packed_only_values_and_lock_contention_is_atomic() {
        let (repository, filesystem, commit, _) = fixture();
        let missing = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        filesystem
            .write(
                Path::new("repo/.git/packed-refs"),
                format!("# pack-refs with: peeled\n{missing} refs/archive/missing\n^{commit}\n")
                    .as_bytes(),
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/.git/refs/tags/light.lock"), b"busy")
            .unwrap();
        assert!(repository.pack_refs(&PackRefsOptions::default()).is_err());
        let unchanged = filesystem.read(Path::new("repo/.git/packed-refs")).unwrap();
        assert!(
            String::from_utf8(unchanged)
                .unwrap()
                .contains("refs/archive/missing")
        );
        filesystem
            .remove_file(Path::new("repo/.git/refs/tags/light.lock"))
            .unwrap();
        repository.pack_refs(&PackRefsOptions::default()).unwrap();
        let packed =
            String::from_utf8(filesystem.read(Path::new("repo/.git/packed-refs")).unwrap())
                .unwrap();
        assert!(packed.contains(&format!("{missing} refs/archive/missing\n^{commit}\n")));
    }
}
