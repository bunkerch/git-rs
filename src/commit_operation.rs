//! Commit the index and advance `HEAD` without assuming host storage.

use std::str::FromStr;

use crate::{
    CommitBuilder, Error, ObjectId, PreviousValue, ReferenceTarget, Repository, Result, Signature,
};

/// Policy and read bounds for creating a commit from the index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOptions {
    /// Permit a non-merge commit whose tree is unchanged from its parent.
    pub allow_empty: bool,
    /// Replace `HEAD`, reusing its parents instead of making it a parent.
    pub amend: bool,
    /// Maximum decoded size of each commit inspected for validation.
    pub max_object_size: usize,
}

impl Default for CommitOptions {
    fn default() -> Self {
        Self {
            allow_empty: false,
            amend: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Create a commit from the stage-zero index and advance `HEAD`.
    ///
    /// The supplied author is explicit, including for amend. An unborn branch
    /// creates a root commit. Amend reuses the current commit's parents. An
    /// active merge adds every `MERGE_HEAD` as a parent and is cleared only
    /// after the ref update succeeds. Cherry-pick, revert, and rebase state must
    /// be completed through their dedicated continuation APIs.
    ///
    /// # Errors
    /// Returns an error for unresolved or intent-to-add entries, an unchanged
    /// tree without `allow_empty`, invalid state/parents, stale `HEAD`, lock
    /// contention, malformed objects, or storage failures.
    pub fn commit_index(
        &self,
        message: &[u8],
        author: &Signature,
        committer: &Signature,
        options: &CommitOptions,
    ) -> Result<ObjectId> {
        self.commit_index_inner(message, author, committer, options, false)
    }

    pub(crate) fn commit_index_during_am(
        &self,
        message: &[u8],
        author: &Signature,
        committer: &Signature,
        options: &CommitOptions,
    ) -> Result<ObjectId> {
        self.commit_index_inner(message, author, committer, options, true)
    }

    fn commit_index_inner(
        &self,
        message: &[u8],
        author: &Signature,
        committer: &Signature,
        options: &CommitOptions,
        during_am: bool,
    ) -> Result<ObjectId> {
        self.ensure_commit_state_is_safe(during_am)?;
        let head = self.read_reference("HEAD")?;
        let expected_head = head.target().clone();
        let current = match head.target() {
            ReferenceTarget::Symbolic(_) => match self.resolve_reference("HEAD") {
                Ok(id) => Some(id),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            },
            ReferenceTarget::Direct(id) => Some(*id),
        };
        if options.amend && current.is_none() {
            return Err(Error::InvalidCommit("cannot amend an unborn branch".into()));
        }

        let index = self.read_index()?;
        let tree = self.write_index_tree(&index)?;
        let current_commit = current
            .map(|id| self.read_commit(id, options.max_object_size))
            .transpose()?;
        let merge_parents = self.read_merge_heads(options.max_object_size)?;
        if options.amend && !merge_parents.is_empty() {
            return Err(Error::InvalidCommit(
                "cannot amend while a merge is in progress".into(),
            ));
        }
        if !options.allow_empty && !options.amend && merge_parents.is_empty() {
            if current_commit
                .as_ref()
                .is_some_and(|commit| commit.tree() == tree)
            {
                return Err(Error::InvalidCommit("nothing to commit".into()));
            }
            if current.is_none() && index.entries().is_empty() {
                return Err(Error::InvalidCommit("nothing to commit".into()));
            }
        }

        let mut builder = CommitBuilder::new(tree, author.clone(), committer.clone());
        if options.amend {
            let amended = current_commit
                .as_ref()
                .ok_or_else(|| Error::InvalidCommit("cannot amend an unborn branch".into()))?;
            for parent in amended.parents() {
                builder = builder.parent(*parent);
            }
        } else {
            if let Some(parent) = current {
                builder = builder.parent(parent);
            }
            for parent in &merge_parents {
                if Some(*parent) != current {
                    builder = builder.parent(*parent);
                }
            }
        }
        let commit = self.write_commit(&builder.message(message.to_vec()).build())?;
        let reflog_message = commit_reflog_message(
            message,
            current.is_none(),
            options.amend,
            !merge_parents.is_empty(),
        );
        self.advance_commit_head(
            &expected_head,
            current,
            commit,
            committer,
            reflog_message.as_bytes(),
        )?;
        if !merge_parents.is_empty() {
            self.clear_commit_merge_state()?;
        }
        Ok(commit)
    }

    fn ensure_commit_state_is_safe(&self, during_am: bool) -> Result<()> {
        for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            if self.filesystem().exists(&self.git_path(name))? {
                return Err(Error::InvalidRepository(format!(
                    "{name} is active; use continue_replay"
                )));
            }
        }
        for name in ["rebase-merge", "rebase-apply"] {
            if during_am && name == "rebase-apply" {
                continue;
            }
            if self.filesystem().exists(&self.git_path(name))? {
                return Err(Error::InvalidRepository(
                    "a rebase is active; use continue_rebase".into(),
                ));
            }
        }
        Ok(())
    }

    fn read_merge_heads(&self, max_object_size: usize) -> Result<Vec<ObjectId>> {
        let data = match self.read_git_file("MERGE_HEAD") {
            Ok(data) => data,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut heads = Vec::new();
        for line in data
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let text = std::str::from_utf8(line)
                .map_err(|_| Error::InvalidRepository("MERGE_HEAD is not ASCII".into()))?;
            let id = ObjectId::from_str(text)
                .map_err(|_| Error::InvalidRepository("MERGE_HEAD is invalid".into()))?;
            self.read_commit(id, max_object_size)?;
            if !heads.contains(&id) {
                heads.push(id);
            }
        }
        if heads.is_empty() {
            return Err(Error::InvalidRepository("MERGE_HEAD is empty".into()));
        }
        Ok(heads)
    }

    fn advance_commit_head(
        &self,
        expected_head: &ReferenceTarget,
        old: Option<ObjectId>,
        new: ObjectId,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        match expected_head {
            ReferenceTarget::Symbolic(branch) => {
                let destination = self.git_path("HEAD");
                let lock = destination.with_extension("lock");
                self.filesystem().write_new(&lock, b"")?;
                let result = (|| {
                    if self.read_reference("HEAD")?.target() != expected_head {
                        return Err(Error::ReferenceConflict("HEAD".into()));
                    }
                    self.update_reference_with_reflog(
                        branch,
                        new,
                        old.map_or(PreviousValue::MustNotExist, PreviousValue::MustExist),
                        committer,
                        message,
                    )?;
                    self.append_reflog(
                        "HEAD",
                        old.unwrap_or_else(ObjectId::null),
                        new,
                        committer,
                        message,
                    )
                })();
                let cleanup = self.filesystem().remove_file(&lock);
                match (result, cleanup) {
                    (Err(error), _) | (Ok(()), Err(error)) => Err(error),
                    (Ok(()), Ok(())) => Ok(()),
                }
            }
            ReferenceTarget::Direct(actual) if Some(*actual) == old => {
                self.update_detached_head(*actual, new, committer, message)
            }
            ReferenceTarget::Direct(_) => Err(Error::ReferenceConflict("HEAD".into())),
        }
    }

    fn update_detached_head(
        &self,
        old: ObjectId,
        new: ObjectId,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        let destination = self.git_path("HEAD");
        let lock = destination.with_extension("lock");
        self.filesystem().write_new(&lock, b"")?;
        let result = (|| {
            match self.read_reference("HEAD")?.target() {
                ReferenceTarget::Direct(actual) if *actual == old => {}
                _ => return Err(Error::ReferenceConflict("HEAD".into())),
            }
            self.filesystem()
                .write(&lock, format!("{new}\n").as_bytes())?;
            self.append_reflog("HEAD", old, new, committer, message)?;
            self.filesystem().rename(&lock, &destination)
        })();
        if result.is_err() {
            let _ = self.filesystem().remove_file(&lock);
        }
        result
    }

    fn clear_commit_merge_state(&self) -> Result<()> {
        for name in ["MERGE_HEAD", "MERGE_MSG"] {
            match self.filesystem().remove_file(&self.git_path(name)) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn commit_reflog_message(message: &[u8], initial: bool, amend: bool, merge: bool) -> String {
    let action = if initial {
        "commit (initial)"
    } else if amend {
        "commit (amend)"
    } else if merge {
        "commit (merge)"
    } else {
        "commit"
    };
    let subject = message
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    format!("{action}: {}", String::from_utf8_lossy(subject))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        FileSystem, Index, IndexEntry, IndexVersion, InitOptions, MemoryFileSystem, ObjectKind,
        ReferenceName, StatData,
    };

    fn fixture() -> (Repository, MemoryFileSystem, Signature) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Committer", "committer@example.com", 100, 0).unwrap();
        (repository, filesystem, signature)
    }

    fn stage(repository: &Repository, contents: &[u8]) -> ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let entry = IndexEntry::new("file", 0o100_644, blob, StatData::default()).unwrap();
        repository
            .write_index(&Index::new(IndexVersion::V2, vec![entry]).unwrap())
            .unwrap();
        blob
    }

    #[test]
    fn commits_unborn_and_normal_branches_with_reflogs() {
        let (repository, _, signature) = fixture();
        stage(&repository, b"one\n");
        let root = repository
            .commit_index(
                b"root subject\n\nbody\n",
                &signature,
                &signature,
                &CommitOptions::default(),
            )
            .unwrap();
        assert!(
            repository
                .read_commit(root, 4096)
                .unwrap()
                .parents()
                .is_empty()
        );
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), root);

        stage(&repository, b"two\n");
        let second = repository
            .commit_index(
                b"second\n",
                &signature,
                &signature,
                &CommitOptions::default(),
            )
            .unwrap();
        assert_eq!(
            repository.read_commit(second, 4096).unwrap().parents(),
            &[root]
        );
        let branch_log = repository.read_reflog("refs/heads/main").unwrap();
        let head_log = repository.read_reflog("HEAD").unwrap();
        assert_eq!(branch_log.len(), 2);
        assert_eq!(head_log.len(), 2);
        assert_eq!(head_log[0].message(), b"commit (initial): root subject");
        assert_eq!(head_log[1].message(), b"commit: second");
    }

    #[test]
    fn rejects_empty_unresolved_and_active_replay_commits() {
        let (repository, filesystem, signature) = fixture();
        assert!(
            repository
                .commit_index(b"empty", &signature, &signature, &CommitOptions::default())
                .is_err()
        );
        let root = repository
            .commit_index(
                b"empty",
                &signature,
                &signature,
                &CommitOptions {
                    allow_empty: true,
                    ..CommitOptions::default()
                },
            )
            .unwrap();
        assert!(
            repository
                .commit_index(b"again", &signature, &signature, &CommitOptions::default())
                .is_err()
        );

        let blob = repository
            .write_object(ObjectKind::Blob, b"conflict")
            .unwrap();
        let entry =
            IndexEntry::with_stage("file", 0o100_644, blob, StatData::default(), 2).unwrap();
        repository
            .write_index(&Index::new(IndexVersion::V2, vec![entry]).unwrap())
            .unwrap();
        assert!(
            repository
                .commit_index(
                    b"unresolved",
                    &signature,
                    &signature,
                    &CommitOptions::default()
                )
                .is_err()
        );
        filesystem
            .write(
                Path::new("repo/.git/CHERRY_PICK_HEAD"),
                format!("{root}\n").as_bytes(),
            )
            .unwrap();
        assert!(
            repository
                .commit_index(b"replay", &signature, &signature, &CommitOptions::default())
                .is_err()
        );
    }

    #[test]
    fn amend_reuses_parents_and_merge_adds_and_clears_heads() {
        let (repository, filesystem, signature) = fixture();
        stage(&repository, b"base");
        let base = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        stage(&repository, b"main");
        let main = repository
            .commit_index(b"main", &signature, &signature, &CommitOptions::default())
            .unwrap();
        let amended = repository
            .commit_index(
                b"amended message",
                &signature,
                &signature,
                &CommitOptions {
                    amend: true,
                    ..CommitOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            repository.read_commit(amended, 4096).unwrap().parents(),
            &[base]
        );

        let tree = repository.read_commit(base, 4096).unwrap().tree();
        let side = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(base)
                    .message(b"side".to_vec())
                    .build(),
            )
            .unwrap();
        filesystem
            .write(
                Path::new("repo/.git/MERGE_HEAD"),
                format!("{side}\n{side}\n").as_bytes(),
            )
            .unwrap();
        filesystem
            .write(Path::new("repo/.git/MERGE_MSG"), b"merge")
            .unwrap();
        let merged = repository
            .commit_index(b"merge", &signature, &signature, &CommitOptions::default())
            .unwrap();
        assert_eq!(
            repository.read_commit(merged, 4096).unwrap().parents(),
            &[amended, side]
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/MERGE_HEAD"))
                .unwrap()
        );
        assert_ne!(main, amended);
    }

    #[test]
    fn commits_on_detached_head_without_moving_a_branch() {
        let (repository, filesystem, signature) = fixture();
        stage(&repository, b"base");
        let base = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        repository
            .write_atomic(Path::new("HEAD"), format!("{base}\n").as_bytes())
            .unwrap();
        stage(&repository, b"detached");
        let detached = repository
            .commit_index(
                b"detached",
                &signature,
                &signature,
                &CommitOptions::default(),
            )
            .unwrap();
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), detached);
        assert_eq!(
            repository
                .resolve_reference(ReferenceName::branch("main").unwrap().as_str())
                .unwrap(),
            base
        );
        assert_eq!(
            filesystem.read(Path::new("repo/.git/HEAD")).unwrap(),
            format!("{detached}\n").as_bytes()
        );
    }

    #[test]
    fn symbolic_head_lock_prevents_a_racing_commit() {
        let (repository, filesystem, signature) = fixture();
        stage(&repository, b"base");
        let base = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        stage(&repository, b"next");
        filesystem
            .write_new(Path::new("repo/.git/HEAD.lock"), b"held")
            .unwrap();
        assert!(
            repository
                .commit_index(b"next", &signature, &signature, &CommitOptions::default())
                .is_err()
        );
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), base);
    }
}
