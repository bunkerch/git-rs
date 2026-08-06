//! Operations that move HEAD, refs, the index, and the worktree together.

use std::path::Path;

use crate::{
    CheckoutOptions, Index, IndexEntry, ObjectId, PreviousValue, ReferenceTarget, Repository,
    Result, Signature, StatData,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Clone, Debug)]
pub struct ResetOptions {
    pub mode: ResetMode,
    pub max_object_size: usize,
}

impl Default for ResetOptions {
    fn default() -> Self {
        Self {
            mode: ResetMode::Mixed,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SwitchOptions {
    pub force: bool,
    pub max_object_size: usize,
}

impl Default for SwitchOptions {
    fn default() -> Self {
        Self {
            force: false,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Reset the current branch or detached HEAD to a commit.
    ///
    /// Soft reset moves only the ref. Mixed reset also replaces the index. Hard
    /// reset forcefully checks out the target tree before moving the ref. Both
    /// HEAD and branch reflogs are updated where applicable.
    ///
    /// # Errors
    /// Returns an error for non-commit targets, corrupt trees, unresolved index
    /// construction, stale refs, lock contention, or filesystem failures.
    pub fn reset(
        &self,
        target: ObjectId,
        options: &ResetOptions,
        committer: &Signature,
    ) -> Result<()> {
        let commit = self.read_commit(target, options.max_object_size)?;
        let old = resolve_head_optional(self)?;
        match options.mode {
            ResetMode::Soft => {}
            ResetMode::Mixed => {
                let current = self.read_index()?;
                let entries = self
                    .flattened_tree(commit.tree(), options.max_object_size)?
                    .into_iter()
                    .map(|entry| {
                        IndexEntry::new(entry.path, entry.raw_mode, entry.id, StatData::default())
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.write_index(&Index::new(current.version(), entries)?)?;
            }
            ResetMode::Hard => {
                self.checkout_tree(
                    commit.tree(),
                    &CheckoutOptions {
                        force: true,
                        max_object_size: options.max_object_size,
                    },
                )?;
            }
        }
        let message = format!("reset: moving to {target}");
        match self.read_reference("HEAD")?.target() {
            ReferenceTarget::Symbolic(branch) => {
                self.update_reference_with_reflog(
                    branch,
                    target,
                    old.map_or(PreviousValue::MustNotExist, PreviousValue::MustExist),
                    committer,
                    message.as_bytes(),
                )?;
                self.append_reflog(
                    "HEAD",
                    old.unwrap_or_else(ObjectId::null),
                    target,
                    committer,
                    message.as_bytes(),
                )
            }
            ReferenceTarget::Direct(_) => self.set_detached_head(
                target,
                old.unwrap_or_else(ObjectId::null),
                committer,
                message.as_bytes(),
            ),
        }
    }

    /// Switch HEAD to an existing branch with checkout protection.
    ///
    /// # Errors
    /// Returns an error for a missing branch, non-commit target, local checkout
    /// conflicts, corrupt objects, or ref/index/filesystem failures.
    pub fn switch_branch(
        &self,
        branch_name: &str,
        options: &SwitchOptions,
        committer: &Signature,
    ) -> Result<ObjectId> {
        let branch = crate::ReferenceName::branch(branch_name)?;
        let target = self.resolve_reference(branch.as_str())?;
        let commit = self.read_commit(target, options.max_object_size)?;
        let old = resolve_head_optional(self)?;
        let old_name = head_label(self)?;
        self.checkout_tree(
            commit.tree(),
            &CheckoutOptions {
                force: options.force,
                max_object_size: options.max_object_size,
            },
        )?;
        self.write_atomic(
            Path::new("HEAD"),
            format!("ref: {}\n", branch.as_str()).as_bytes(),
        )?;
        let message = format!("checkout: moving from {old_name} to {branch_name}");
        self.append_reflog(
            "HEAD",
            old.unwrap_or_else(ObjectId::null),
            target,
            committer,
            message.as_bytes(),
        )?;
        Ok(target)
    }

    /// Detach HEAD at a commit with checkout protection.
    ///
    /// # Errors
    /// Returns an error for non-commit targets, local checkout conflicts,
    /// corrupt objects, or ref/index/filesystem failures.
    pub fn switch_detached(
        &self,
        target: ObjectId,
        options: &SwitchOptions,
        committer: &Signature,
    ) -> Result<()> {
        let commit = self.read_commit(target, options.max_object_size)?;
        let old = resolve_head_optional(self)?.unwrap_or_else(ObjectId::null);
        let old_name = head_label(self)?;
        self.checkout_tree(
            commit.tree(),
            &CheckoutOptions {
                force: options.force,
                max_object_size: options.max_object_size,
            },
        )?;
        let message = format!("checkout: moving from {old_name} to {target}");
        self.set_detached_head(target, old, committer, message.as_bytes())
    }

    fn set_detached_head(
        &self,
        target: ObjectId,
        old: ObjectId,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        self.write_atomic(Path::new("HEAD"), format!("{target}\n").as_bytes())?;
        self.append_reflog("HEAD", old, target, committer, message)
    }
}

fn head_label(repository: &Repository) -> Result<String> {
    match repository.read_reference("HEAD")?.target() {
        ReferenceTarget::Symbolic(name) => Ok(name
            .as_str()
            .strip_prefix("refs/heads/")
            .unwrap_or(name.as_str())
            .to_owned()),
        ReferenceTarget::Direct(id) => Ok(id.to_string()),
    }
}

fn resolve_head_optional(repository: &Repository) -> Result<Option<ObjectId>> {
    match repository.resolve_reference("HEAD") {
        Ok(id) => Ok(Some(id)),
        Err(crate::Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, ObjectKind,
        ReferenceName, Tree, TreeEntry,
    };

    fn fixture() -> (Repository, MemoryFileSystem, ObjectId, ObjectId, Signature) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Test", "test@example.com", 123, 0).unwrap();
        let mut commits = Vec::new();
        for contents in [b"one".as_slice(), b"two".as_slice()] {
            let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
            let tree = repository
                .write_tree(
                    &Tree::new(vec![
                        TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                    ])
                    .unwrap(),
                )
                .unwrap();
            let mut builder = CommitBuilder::new(tree, signature.clone(), signature.clone())
                .message(contents.to_vec());
            if let Some(parent) = commits.last().copied() {
                builder = builder.parent(parent);
            }
            commits.push(repository.write_commit(&builder.build()).unwrap());
        }
        repository
            .update_reference_with_reflog(
                &ReferenceName::branch("main").unwrap(),
                commits[1],
                PreviousValue::MustNotExist,
                &signature,
                b"commit: two",
            )
            .unwrap();
        repository
            .checkout_tree(
                repository.read_commit(commits[1], 4096).unwrap().tree(),
                &CheckoutOptions::default(),
            )
            .unwrap();
        (repository, fs, commits[0], commits[1], signature)
    }

    #[test]
    fn soft_mixed_and_hard_reset_move_the_expected_layers() {
        let (repository, fs, first, second, signature) = fixture();
        repository
            .reset(
                first,
                &ResetOptions {
                    mode: ResetMode::Soft,
                    ..ResetOptions::default()
                },
                &signature,
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"two");
        assert!(
            repository
                .status(&crate::StatusOptions::default())
                .unwrap()
                .entries()[0]
                .index_change()
                .is_some()
        );

        repository
            .reset(
                second,
                &ResetOptions {
                    mode: ResetMode::Mixed,
                    ..ResetOptions::default()
                },
                &signature,
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"two");
        repository
            .reset(
                first,
                &ResetOptions {
                    mode: ResetMode::Hard,
                    ..ResetOptions::default()
                },
                &signature,
            )
            .unwrap();
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"one");
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), first);
        assert_eq!(repository.read_reflog("HEAD").unwrap().len(), 3);
    }

    #[test]
    fn switches_branches_and_detaches_head() {
        let (repository, fs, first, second, signature) = fixture();
        repository.create_branch("old", first, false).unwrap();
        repository
            .switch_branch("old", &SwitchOptions::default(), &signature)
            .unwrap();
        assert_eq!(
            fs.read(Path::new("repo/.git/HEAD")).unwrap(),
            b"ref: refs/heads/old\n"
        );
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"one");
        repository
            .switch_detached(second, &SwitchOptions::default(), &signature)
            .unwrap();
        assert_eq!(
            fs.read(Path::new("repo/.git/HEAD")).unwrap(),
            format!("{second}\n").as_bytes()
        );
        assert_eq!(repository.read_reflog("HEAD").unwrap().len(), 2);
    }
}
