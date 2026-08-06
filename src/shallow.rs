//! Git-compatible shallow commit boundary storage.

use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;

use crate::{Error, ObjectId, ObjectKind, Repository, Result};

/// Bounds for reading or validating `.git/shallow`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShallowOptions {
    pub max_commits: usize,
    pub max_object_size: usize,
}

impl Default for ShallowOptions {
    fn default() -> Self {
        Self {
            max_commits: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Read the checksum-shaped commit IDs declared as shallow boundaries.
    ///
    /// # Errors
    /// Returns an error for malformed, duplicate, excessive, missing, or
    /// non-commit object IDs, or storage failures.
    pub fn shallow_commits(&self, options: &ShallowOptions) -> Result<BTreeSet<ObjectId>> {
        let data = match self.read_git_file("shallow") {
            Ok(data) => data,
            Err(Error::NotFound(_)) => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        let mut commits = BTreeSet::new();
        for line in data.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value = std::str::from_utf8(line)
                .map_err(|_| Error::InvalidRepository("shallow ID is not ASCII".into()))?;
            let id = ObjectId::from_str(value)
                .map_err(|_| Error::InvalidRepository("invalid shallow object ID".into()))?;
            if !commits.insert(id) {
                return Err(Error::InvalidRepository(
                    "duplicate shallow object ID".into(),
                ));
            }
            if commits.len() > options.max_commits {
                return Err(Error::InvalidRepository(
                    "shallow commit count exceeds limit".into(),
                ));
            }
            if self.read_object(id, options.max_object_size)?.kind() != ObjectKind::Commit {
                return Err(Error::InvalidRepository(
                    "shallow boundary is not a commit".into(),
                ));
            }
        }
        Ok(commits)
    }

    /// Atomically replace the shallow boundary set, or remove it when empty.
    ///
    /// # Errors
    /// Returns an error for a non-commit/excessive boundary or storage failure.
    pub fn write_shallow_commits(
        &self,
        commits: &BTreeSet<ObjectId>,
        options: &ShallowOptions,
    ) -> Result<()> {
        if commits.len() > options.max_commits {
            return Err(Error::InvalidRepository(
                "shallow commit count exceeds limit".into(),
            ));
        }
        for id in commits {
            if self.read_object(*id, options.max_object_size)?.kind() != ObjectKind::Commit {
                return Err(Error::InvalidRepository(
                    "shallow boundary is not a commit".into(),
                ));
            }
        }
        if commits.is_empty() {
            return match self.filesystem().remove_file(&self.git_path("shallow")) {
                Ok(()) | Err(Error::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            };
        }
        let mut encoded = Vec::with_capacity(commits.len().saturating_mul(41));
        for id in commits {
            encoded.extend_from_slice(id.to_string().as_bytes());
            encoded.push(b'\n');
        }
        self.write_atomic(Path::new("shallow"), &encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitBuilder, InitOptions, MemoryFileSystem, Signature, Tree};

    #[test]
    fn boundaries_round_trip_and_empty_removes_file() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let signature = Signature::new("S", "s@example.com", 1, 0).unwrap();
        let id = repository
            .write_commit(&CommitBuilder::new(tree, signature.clone(), signature).build())
            .unwrap();
        let commits = BTreeSet::from([id]);
        repository
            .write_shallow_commits(&commits, &ShallowOptions::default())
            .unwrap();
        assert_eq!(
            repository
                .shallow_commits(&ShallowOptions::default())
                .unwrap(),
            commits
        );
        repository
            .write_shallow_commits(&BTreeSet::new(), &ShallowOptions::default())
            .unwrap();
        assert!(
            repository
                .shallow_commits(&ShallowOptions::default())
                .unwrap()
                .is_empty()
        );
    }
}
