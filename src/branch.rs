//! Local branch deletion and rename across refs, worktrees, reflogs, and config.

use std::path::{Path, PathBuf};

use crate::{
    Error, GraphOptions, ObjectId, PreviousValue, Reference, ReferenceEdit, ReferenceName,
    ReferenceTarget, Repository, Result, Signature,
};

/// Safety and graph bounds for deleting a local branch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeleteBranchOptions {
    /// Permit deletion when the tip is not merged into `HEAD`.
    pub force: bool,
    pub graph: GraphOptions,
}

/// Destination replacement policy for branch rename.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenameBranchOptions {
    pub force: bool,
}

impl Repository {
    /// Delete a local branch after checked-out and mergedness validation.
    ///
    /// The branch cannot be checked out in the main or any linked worktree,
    /// even with `force`. Without force its tip must be an ancestor of `HEAD`.
    /// The ref/reflog and matching `[branch "name"]` config are removed.
    ///
    /// # Errors
    /// Returns an error for invalid/missing/symbolic branches, checked-out
    /// worktrees, unmerged tips, graph bounds, stale refs, or storage failures.
    pub fn delete_branch(&self, name: &str, options: &DeleteBranchOptions) -> Result<ObjectId> {
        let branch = ReferenceName::branch(name)?;
        self.ensure_branch_available(&branch)?;
        let reference = self.read_reference(branch.as_str())?;
        let target = match reference.target() {
            ReferenceTarget::Direct(id) => *id,
            ReferenceTarget::Symbolic(_) => {
                return Err(Error::InvalidReference(
                    "cannot delete a symbolic local branch".into(),
                ));
            }
        };
        if !options.force {
            let head = self.resolve_reference("HEAD")?;
            self.read_commit(target, options.graph.max_object_size)?;
            self.read_commit(head, options.graph.max_object_size)?;
            if !self.is_ancestor(target, head, &options.graph)? {
                return Err(Error::ReferenceConflict(format!(
                    "branch `{name}` is not fully merged into HEAD"
                )));
            }
        }
        self.delete_reference(&branch, target)?;
        let mut config = self.read_config()?;
        config.remove_subsection("branch", name.as_bytes())?;
        self.write_config(&config)?;
        Ok(target)
    }

    /// Rename a local branch, preserving its tip and reflog and updating every
    /// worktree whose `HEAD` points at the old name.
    ///
    /// # Errors
    /// Returns an error for invalid/missing/symbolic names, an occupied
    /// destination without force, a destination checked out in any worktree,
    /// stale ref transactions, malformed config, or storage failures.
    pub fn rename_branch(
        &self,
        old: &str,
        new: &str,
        options: &RenameBranchOptions,
        committer: &Signature,
    ) -> Result<Reference> {
        let old_name = ReferenceName::branch(old)?;
        let new_name = ReferenceName::branch(new)?;
        if old_name == new_name {
            return self.branch(old);
        }
        let old_reference = self.read_reference(old_name.as_str())?;
        let target = match old_reference.target() {
            ReferenceTarget::Direct(id) => *id,
            ReferenceTarget::Symbolic(_) => {
                return Err(Error::InvalidReference(
                    "cannot rename a symbolic local branch".into(),
                ));
            }
        };
        let destination = match self.read_reference(new_name.as_str()) {
            Ok(reference) => {
                if !options.force {
                    return Err(Error::ReferenceConflict(new_name.to_string()));
                }
                self.ensure_branch_available(&new_name)?;
                match reference.target() {
                    ReferenceTarget::Direct(id) => Some(*id),
                    ReferenceTarget::Symbolic(_) => {
                        return Err(Error::ReferenceConflict(new_name.to_string()));
                    }
                }
            }
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let heads = self.branch_head_locations(&old_name)?;
        let old_log = match self
            .filesystem()
            .read(&self.common_dir().join("logs").join(old_name.as_str()))
        {
            Ok(contents) => contents,
            Err(Error::NotFound(_)) => Vec::new(),
            Err(error) => return Err(error),
        };
        self.apply_reference_transaction(&[
            ReferenceEdit::update(
                new_name.clone(),
                target,
                destination.map_or(PreviousValue::MustNotExist, PreviousValue::MustExist),
            ),
            ReferenceEdit::delete(old_name.clone(), target),
        ])?;

        let new_log = Path::new("logs").join(new_name.as_str());
        self.write_atomic(&new_log, &old_log)?;
        self.append_reflog(
            new_name.as_str(),
            target,
            target,
            committer,
            format!("Branch: renamed {old_name} to {new_name}").as_bytes(),
        )?;
        for location in heads {
            write_storage_atomic(self, &location, format!("ref: {new_name}\n").as_bytes())?;
        }

        let mut config = self.read_config()?;
        if options.force {
            config.remove_subsection("branch", new.as_bytes())?;
        }
        config.rename_subsection("branch", old.as_bytes(), new.as_bytes())?;
        self.write_config(&config)?;
        self.branch(new)
    }

    fn branch_head_locations(&self, branch: &ReferenceName) -> Result<Vec<PathBuf>> {
        let mut locations = Vec::new();
        let main = self.common_dir().join("HEAD");
        if head_points_to(&self.filesystem().read(&main)?, branch) {
            locations.push(main);
        }
        let root = self.common_dir().join("worktrees");
        let worktrees = match self.filesystem().read_dir(&root) {
            Ok(entries) => entries,
            Err(Error::NotFound(_)) => return Ok(locations),
            Err(error) => return Err(error),
        };
        for worktree in worktrees {
            let head = root.join(worktree).join("HEAD");
            if head_points_to(&self.filesystem().read(&head)?, branch) {
                locations.push(head);
            }
        }
        Ok(locations)
    }
}

fn head_points_to(contents: &[u8], branch: &ReferenceName) -> bool {
    contents
        .strip_prefix(b"ref: ")
        .map(|value| value.strip_suffix(b"\n").unwrap_or(value))
        == Some(branch.as_str().as_bytes())
}

fn write_storage_atomic(
    repository: &Repository,
    destination: &Path,
    contents: &[u8],
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        repository.filesystem().create_dir_all(parent)?;
    }
    let lock = destination.with_extension("lock");
    repository.filesystem().write_new(&lock, contents)?;
    if let Err(error) = repository.filesystem().rename(&lock, destination) {
        let _ = repository.filesystem().remove_file(&lock);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        AddWorktreeOptions, CommitBuilder, FileSystem, InitOptions, MemoryFileSystem, Tree,
        WorktreeTarget,
    };

    fn repository() -> (Repository, MemoryFileSystem, Signature, ObjectId, ObjectId) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("Branch", "branch@example.com", 100, 0).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let base = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .message(b"base\n".to_vec())
                    .build(),
            )
            .unwrap();
        let head = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(base)
                    .message(b"head\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository
            .update_reference_with_reflog(
                &ReferenceName::branch("main").unwrap(),
                head,
                PreviousValue::MustNotExist,
                &signature,
                b"branch: main",
            )
            .unwrap();
        (repository, filesystem, signature, base, head)
    }

    #[test]
    fn deletes_only_merged_non_checked_out_branches_without_force() {
        let (repository, _, _, base, head) = repository();
        repository.create_branch("merged", base, false).unwrap();
        let mut config = repository.read_config().unwrap();
        config.set("branch.merged.remote", b"origin").unwrap();
        repository.write_config(&config).unwrap();
        assert_eq!(
            repository
                .delete_branch("merged", &DeleteBranchOptions::default())
                .unwrap(),
            base
        );
        assert!(repository.branch("merged").is_err());
        assert!(
            repository
                .read_config()
                .unwrap()
                .get("branch.merged.remote")
                .unwrap()
                .is_none()
        );

        let tree = repository.read_commit(base, 4096).unwrap().tree();
        let signature = Signature::new("Side", "side@example.com", 200, 0).unwrap();
        let divergent = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .parent(base)
                    .message(b"divergent\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository.create_branch("topic", divergent, false).unwrap();
        assert!(
            repository
                .delete_branch("topic", &DeleteBranchOptions::default())
                .is_err()
        );
        assert_eq!(
            repository.resolve_reference("refs/heads/topic").unwrap(),
            divergent
        );
        repository
            .delete_branch(
                "topic",
                &DeleteBranchOptions {
                    force: true,
                    ..DeleteBranchOptions::default()
                },
            )
            .unwrap();
        assert!(
            repository
                .delete_branch(
                    "main",
                    &DeleteBranchOptions {
                        force: true,
                        ..DeleteBranchOptions::default()
                    },
                )
                .is_err()
        );
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), head);
    }

    #[test]
    fn renames_current_branch_reflog_head_and_config() {
        let (repository, filesystem, signature, _, head) = repository();
        let mut config = repository.read_config().unwrap();
        config.set("branch.main.remote", b"origin").unwrap();
        config.set("branch.main.merge", b"refs/heads/main").unwrap();
        repository.write_config(&config).unwrap();

        let renamed = repository
            .rename_branch("main", "trunk", &RenameBranchOptions::default(), &signature)
            .unwrap();
        assert_eq!(renamed.name(), "refs/heads/trunk");
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), head);
        assert!(repository.branch("main").is_err());
        assert_eq!(
            filesystem.read(Path::new("repo/.git/HEAD")).unwrap(),
            b"ref: refs/heads/trunk\n"
        );
        let config = repository.read_config().unwrap();
        assert!(config.get("branch.main.remote").unwrap().is_none());
        assert_eq!(
            config.get("branch.trunk.remote").unwrap().unwrap().value(),
            Some(b"origin".as_slice())
        );
        let reflog = repository.read_reflog("refs/heads/trunk").unwrap();
        assert_eq!(reflog.len(), 2);
        assert!(reflog[1].message().starts_with(b"Branch: renamed"));
    }

    #[test]
    fn updates_linked_worktree_heads_and_protects_checked_out_destination() {
        let (repository, filesystem, signature, base, head) = repository();
        repository.create_branch("topic", base, false).unwrap();
        repository
            .add_worktree(
                "topic-work",
                "topic-work",
                &WorktreeTarget::Branch("topic".to_owned()),
                &AddWorktreeOptions::default(),
            )
            .unwrap();
        repository
            .rename_branch(
                "topic",
                "renamed",
                &RenameBranchOptions::default(),
                &signature,
            )
            .unwrap();
        assert_eq!(
            filesystem
                .read(Path::new("repo/.git/worktrees/topic-work/HEAD"))
                .unwrap(),
            b"ref: refs/heads/renamed\n"
        );
        assert!(
            repository
                .delete_branch(
                    "renamed",
                    &DeleteBranchOptions {
                        force: true,
                        ..DeleteBranchOptions::default()
                    }
                )
                .is_err()
        );

        repository.create_branch("replace", head, false).unwrap();
        assert!(
            repository
                .rename_branch(
                    "replace",
                    "renamed",
                    &RenameBranchOptions { force: true },
                    &signature,
                )
                .is_err()
        );
    }
}
