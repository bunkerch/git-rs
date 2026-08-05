//! Git references and reference transactions.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{Error, ObjectId, Repository, Result};

const SYMBOLIC_REF_MAX_DEPTH: usize = 5;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReferenceName(String);

impl ReferenceName {
    /// Parse a fully-qualified reference name beginning with `refs/`.
    ///
    /// # Errors
    /// Returns an error when `name` violates Git's reference grammar.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if !name.starts_with("refs/") || !is_valid_refname(&name, false) {
            return Err(Error::InvalidReferenceName(name));
        }
        Ok(Self(name))
    }

    /// Construct `refs/heads/<name>` after validating the branch name.
    ///
    /// # Errors
    /// Returns an error when the resulting name violates Git's ref grammar.
    pub fn branch(name: &str) -> Result<Self> {
        Self::new(format!("refs/heads/{name}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReferenceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceTarget {
    Direct(ObjectId),
    Symbolic(ReferenceName),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    name: String,
    target: ReferenceTarget,
}

impl Reference {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn target(&self) -> &ReferenceTarget {
        &self.target
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviousValue {
    Any,
    MustNotExist,
    MustExist(ObjectId),
}

impl Repository {
    /// Create or reset a branch to `target`.
    ///
    /// # Errors
    /// Returns an error for an invalid branch name, an existing branch when
    /// `force` is false, lock contention, or a storage failure.
    pub fn create_branch(&self, name: &str, target: ObjectId, force: bool) -> Result<Reference> {
        let name = ReferenceName::branch(name)?;
        let previous = if force {
            PreviousValue::Any
        } else {
            PreviousValue::MustNotExist
        };
        self.update_reference(&name, target, previous)?;
        Ok(Reference {
            name: name.0,
            target: ReferenceTarget::Direct(target),
        })
    }

    /// Read a branch by its short name.
    ///
    /// # Errors
    /// Returns an error for an invalid name, missing branch, malformed ref, or
    /// storage failure.
    pub fn branch(&self, name: &str) -> Result<Reference> {
        self.read_reference(ReferenceName::branch(name)?.as_str())
    }

    /// List loose and packed branches in bytewise name order.
    ///
    /// Loose references take precedence over duplicate packed entries, matching
    /// the files reference backend.
    ///
    /// # Errors
    /// Returns an error for malformed references or storage failures.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    pub fn branches(&self) -> Result<Vec<Reference>> {
        let mut branches = self.packed_references_with_prefix("refs/heads/")?;
        let root = self.git_path("refs/heads");
        let mut directories = vec![(root, String::from("refs/heads"))];
        while let Some((directory, prefix)) = directories.pop() {
            for child in self.filesystem().read_dir(&directory)? {
                let path = directory.join(&child);
                let name = format!("{prefix}/{}", child.to_string_lossy());
                let metadata = self.filesystem().metadata(&path)?;
                if metadata.is_dir() {
                    directories.push((path, name));
                } else if metadata.is_file() && !name.ends_with(".lock") {
                    branches.insert(name.clone(), self.read_loose_reference(&name)?);
                }
            }
        }
        Ok(branches.into_values().collect())
    }

    /// Read a loose or packed reference without following a symbolic target.
    ///
    /// `HEAD` is accepted as a pseudoref; other names must be fully-qualified.
    ///
    /// # Errors
    /// Returns an error for an invalid name, malformed reference, or storage failure.
    pub fn read_reference(&self, name: &str) -> Result<Reference> {
        validate_read_name(name)?;
        match self.read_loose_reference(name) {
            Ok(reference) => Ok(reference),
            Err(Error::NotFound(_)) if name.starts_with("refs/") => {
                self.read_packed_reference(name)
            }
            Err(error) => Err(error),
        }
    }

    /// Resolve a reference through symbolic references to an object ID.
    ///
    /// # Errors
    /// Returns an error for missing or malformed references, or after five
    /// symbolic hops (matching Git's `SYMREF_MAXDEPTH`).
    pub fn resolve_reference(&self, name: &str) -> Result<ObjectId> {
        let mut current = name.to_owned();
        for _ in 0..SYMBOLIC_REF_MAX_DEPTH {
            match self.read_reference(&current)?.target {
                ReferenceTarget::Direct(id) => return Ok(id),
                ReferenceTarget::Symbolic(target) => current = target.0,
            }
        }
        Err(Error::SymbolicReferenceLoop(name.to_owned()))
    }

    /// Atomically create or update a direct reference.
    ///
    /// `previous` provides compare-and-swap semantics while the canonical lock
    /// is held, preventing lost updates between concurrent writers.
    ///
    /// # Errors
    /// Returns an error for an invalid name, lock contention, a previous-value
    /// mismatch, or a storage failure.
    pub fn update_reference(
        &self,
        name: &ReferenceName,
        new: ObjectId,
        previous: PreviousValue,
    ) -> Result<()> {
        if new.is_null() {
            return Err(Error::InvalidReference(
                "a ref cannot point to the null object ID".into(),
            ));
        }
        let destination = self.git_path(name.as_str());
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let lock = lock_path(&destination);
        self.filesystem().write_new(&lock, b"")?;

        let result = (|| {
            let actual = match self.read_reference(name.as_str()) {
                Ok(reference) => match reference.target {
                    ReferenceTarget::Direct(id) => Some(id),
                    ReferenceTarget::Symbolic(_) => {
                        return Err(Error::ReferenceConflict(name.0.clone()));
                    }
                },
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            let matches = match previous {
                PreviousValue::Any => true,
                PreviousValue::MustNotExist => actual.is_none(),
                PreviousValue::MustExist(expected) => actual == Some(expected),
            };
            if !matches {
                return Err(Error::ReferenceConflict(name.0.clone()));
            }

            let mut contents = new.to_hex().to_vec();
            contents.push(b'\n');
            self.filesystem().write(&lock, &contents)?;
            self.filesystem().rename(&lock, &destination)
        })();

        if result.is_err() {
            let _ = self.filesystem().remove_file(&lock);
        }
        result
    }

    fn read_loose_reference(&self, name: &str) -> Result<Reference> {
        let path = self.git_path(name);
        let contents = self.filesystem().read(&path)?;
        let text = std::str::from_utf8(&contents)
            .map_err(|_| Error::InvalidReference(format!("{name} is not UTF-8")))?
            .trim_end_matches(|character: char| character.is_ascii_whitespace());
        let target = if let Some(referent) = text.strip_prefix("ref: ") {
            ReferenceTarget::Symbolic(ReferenceName::new(referent.to_owned())?)
        } else {
            ReferenceTarget::Direct(ObjectId::from_str(text)?)
        };
        Ok(Reference {
            name: name.to_owned(),
            target,
        })
    }

    fn read_packed_reference(&self, name: &str) -> Result<Reference> {
        self.packed_references_with_prefix(name)?
            .remove(name)
            .ok_or_else(|| Error::NotFound(PathBuf::from(name)))
    }

    fn packed_references_with_prefix(&self, prefix: &str) -> Result<BTreeMap<String, Reference>> {
        let contents = match self.filesystem().read(&self.git_path("packed-refs")) {
            Ok(contents) => contents,
            Err(Error::NotFound(_)) => return Ok(BTreeMap::new()),
            Err(error) => return Err(error),
        };
        let mut references = BTreeMap::new();
        for line in contents.split(|byte| *byte == b'\n') {
            if line.is_empty() || matches!(line[0], b'#' | b'^') {
                continue;
            }
            let Some(separator) = line.iter().position(|byte| *byte == b' ') else {
                return Err(Error::InvalidReference(
                    "malformed packed-refs entry".into(),
                ));
            };
            let packed_name = std::str::from_utf8(&line[separator + 1..])
                .map_err(|_| Error::InvalidReference("non-UTF-8 packed ref name".into()))?;
            if packed_name.starts_with(prefix) {
                let hex = std::str::from_utf8(&line[..separator])
                    .map_err(|_| Error::InvalidReference("non-UTF-8 packed object ID".into()))?;
                ReferenceName::new(packed_name.to_owned())?;
                references.insert(
                    packed_name.to_owned(),
                    Reference {
                        name: packed_name.to_owned(),
                        target: ReferenceTarget::Direct(ObjectId::from_str(hex)?),
                    },
                );
            }
        }
        Ok(references)
    }
}

fn validate_read_name(name: &str) -> Result<()> {
    if name == "HEAD" || (name.starts_with("refs/") && is_valid_refname(name, false)) {
        Ok(())
    } else {
        Err(Error::InvalidReferenceName(name.to_owned()))
    }
}

// Git's grammar reserves the exact lowercase `.lock` suffix.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(crate) fn is_valid_refname(name: &str, allow_one_level: bool) -> bool {
    if name == "@" || name.is_empty() || name.ends_with('/') || name.ends_with('.') {
        return false;
    }
    let mut components = 0;
    for component in name.split('/') {
        components += 1;
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with(".lock")
            || component.contains("..")
            || component.contains("@{")
            || component.bytes().any(|byte| {
                byte < 0x20
                    || byte == 0x7f
                    || matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
        {
            return false;
        }
    }
    allow_one_level || components >= 2
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem};

    const FIRST: &str = "1111111111111111111111111111111111111111";
    const SECOND: &str = "2222222222222222222222222222222222222222";

    fn repository() -> (Repository, MemoryFileSystem) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        (repository, fs)
    }

    #[test]
    fn matches_git_refname_restrictions() {
        for valid in [
            "refs/heads/main",
            "refs/tags/v1.0",
            "refs/heads/topic.locked",
        ] {
            assert!(ReferenceName::new(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "main",
            "refs/heads/",
            "refs//main",
            "refs/heads/.hidden",
            "refs/heads/a..b",
            "refs/heads/a@{b",
            "refs/heads/a.lock",
            "refs/heads/a?b",
            "refs/heads/a\\b",
        ] {
            assert!(ReferenceName::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn resolves_head_to_a_loose_branch() {
        let (repository, fs) = repository();
        fs.write(
            Path::new("repo/.git/refs/heads/main"),
            format!("{FIRST}\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(
            repository.resolve_reference("HEAD").unwrap(),
            ObjectId::from_str(FIRST).unwrap()
        );
    }

    #[test]
    fn falls_back_to_packed_refs_and_ignores_peeled_lines() {
        let (repository, fs) = repository();
        let packed = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{FIRST} refs/tags/v1\n^{SECOND}\n"
        );
        fs.write(Path::new("repo/.git/packed-refs"), packed.as_bytes())
            .unwrap();
        assert_eq!(
            repository.resolve_reference("refs/tags/v1").unwrap(),
            ObjectId::from_str(FIRST).unwrap()
        );
    }

    #[test]
    fn detects_symbolic_reference_cycles_at_git_depth_limit() {
        let (repository, fs) = repository();
        fs.write(Path::new("repo/.git/refs/heads/a"), b"ref: refs/heads/b\n")
            .unwrap();
        fs.write(Path::new("repo/.git/refs/heads/b"), b"ref: refs/heads/a\n")
            .unwrap();
        assert!(matches!(
            repository.resolve_reference("refs/heads/a"),
            Err(Error::SymbolicReferenceLoop(_))
        ));
    }

    #[test]
    fn compare_and_swap_update_rejects_a_stale_writer_and_cleans_lock() {
        let (repository, fs) = repository();
        let name = ReferenceName::branch("main").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        repository
            .update_reference(&name, first, PreviousValue::MustNotExist)
            .unwrap();
        assert!(matches!(
            repository.update_reference(&name, second, PreviousValue::MustNotExist),
            Err(Error::ReferenceConflict(_))
        ));
        assert_eq!(repository.resolve_reference(name.as_str()).unwrap(), first);
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/main.lock"))
                .unwrap()
        );
    }

    #[test]
    fn update_ref_uses_canonical_dot_lock() {
        let (repository, fs) = repository();
        let name = ReferenceName::branch("topic").unwrap();
        fs.write_new(Path::new("repo/.git/refs/heads/topic.lock"), b"busy")
            .unwrap();
        assert!(matches!(
            repository.update_reference(
                &name,
                ObjectId::from_str(FIRST).unwrap(),
                PreviousValue::Any
            ),
            Err(Error::AlreadyExists(_))
        ));
    }

    #[test]
    fn creates_and_lists_nested_loose_and_packed_branches() {
        let (repository, fs) = repository();
        let first = ObjectId::from_str(FIRST).unwrap();
        repository.create_branch("topic/one", first, false).unwrap();
        fs.write(
            Path::new("repo/.git/packed-refs"),
            format!("{SECOND} refs/heads/packed\n{SECOND} refs/tags/not-a-branch\n").as_bytes(),
        )
        .unwrap();

        let branches = repository.branches().unwrap();
        assert_eq!(
            branches.iter().map(Reference::name).collect::<Vec<_>>(),
            ["refs/heads/packed", "refs/heads/topic/one"]
        );
        assert_eq!(
            repository.branch("topic/one").unwrap().target(),
            &ReferenceTarget::Direct(first)
        );
    }

    #[test]
    fn create_branch_requires_force_to_replace_an_existing_branch() {
        let (repository, _) = repository();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        repository.create_branch("topic", first, false).unwrap();
        assert!(matches!(
            repository.create_branch("topic", second, false),
            Err(Error::ReferenceConflict(_))
        ));
        repository.create_branch("topic", second, true).unwrap();
        assert_eq!(
            repository.resolve_reference("refs/heads/topic").unwrap(),
            second
        );
    }
}
