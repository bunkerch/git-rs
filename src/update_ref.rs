//! High-level compare-and-swap commands for direct and symbolic references.

use crate::{
    Error, ObjectId, PreviousReferenceValue, ReferenceName, ReferenceTarget,
    ReferenceTransactionEdit, Repository, Result,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRefCommand {
    Update {
        name: String,
        new: ObjectId,
        old: Option<ObjectId>,
        no_deref: bool,
    },
    Create {
        name: String,
        new: ObjectId,
        no_deref: bool,
    },
    Delete {
        name: String,
        old: Option<ObjectId>,
        no_deref: bool,
    },
    Verify {
        name: String,
        old: Option<ObjectId>,
        no_deref: bool,
    },
    SymbolicUpdate {
        name: String,
        new: ReferenceName,
        old: Option<ReferenceTarget>,
    },
    SymbolicCreate {
        name: String,
        new: ReferenceName,
    },
    SymbolicDelete {
        name: String,
        old: Option<ReferenceName>,
    },
    SymbolicVerify {
        name: String,
        old: Option<ReferenceName>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateRefOptions {
    pub max_commands: usize,
    pub max_object_size: usize,
}

impl Default for UpdateRefOptions {
    fn default() -> Self {
        Self {
            max_commands: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Validate and atomically apply direct and symbolic update-ref commands.
    ///
    /// Direct commands dereference symbolic names unless `no_deref` is set.
    /// Every new direct target must exist as a valid repository object.
    ///
    /// # Errors
    /// Returns an error for exceeded limits, missing/corrupt new objects,
    /// invalid names, duplicate destinations, stale preconditions, symbolic
    /// loops, lock contention, or storage failure. No ref changes when
    /// validation or transaction preparation fails.
    pub fn update_refs(
        &self,
        commands: &[UpdateRefCommand],
        options: &UpdateRefOptions,
    ) -> Result<usize> {
        if commands.len() > options.max_commands {
            return update_error("update-ref command count exceeds limit");
        }
        let mut edits = Vec::with_capacity(commands.len());
        for command in commands {
            edits.push(self.update_ref_edit(command, options.max_object_size)?);
        }
        self.apply_mixed_reference_transaction(&edits)?;
        Ok(edits.len())
    }

    fn update_ref_edit(
        &self,
        command: &UpdateRefCommand,
        max_object_size: usize,
    ) -> Result<ReferenceTransactionEdit> {
        match command {
            UpdateRefCommand::Update {
                name,
                new,
                old,
                no_deref,
            } => {
                let destination = self.update_ref_destination(name, *no_deref)?;
                self.validate_update_ref_target(&destination, *new, max_object_size)?;
                Ok(ReferenceTransactionEdit::update(
                    destination,
                    ReferenceTarget::Direct(*new),
                    old.map_or(PreviousReferenceValue::Any, |id| {
                        PreviousReferenceValue::MustExist(ReferenceTarget::Direct(id))
                    }),
                ))
            }
            UpdateRefCommand::Create {
                name,
                new,
                no_deref,
            } => {
                let destination = self.update_ref_destination(name, *no_deref)?;
                self.validate_update_ref_target(&destination, *new, max_object_size)?;
                Ok(ReferenceTransactionEdit::update(
                    destination,
                    ReferenceTarget::Direct(*new),
                    PreviousReferenceValue::MustNotExist,
                ))
            }
            UpdateRefCommand::Delete {
                name,
                old,
                no_deref,
            } => Ok(ReferenceTransactionEdit::delete(
                self.update_ref_destination(name, *no_deref)?,
                old.map_or(PreviousReferenceValue::Any, |id| {
                    PreviousReferenceValue::MustExist(ReferenceTarget::Direct(id))
                }),
            )),
            UpdateRefCommand::Verify {
                name,
                old,
                no_deref,
            } => Ok(ReferenceTransactionEdit::verify(
                self.update_ref_destination(name, *no_deref)?,
                old.map_or(PreviousReferenceValue::MustNotExist, |id| {
                    PreviousReferenceValue::MustExist(ReferenceTarget::Direct(id))
                }),
            )),
            UpdateRefCommand::SymbolicUpdate { name, new, old } => {
                Ok(ReferenceTransactionEdit::update(
                    name,
                    ReferenceTarget::Symbolic(new.clone()),
                    old.as_ref()
                        .map_or(PreviousReferenceValue::Any, |target| match target {
                            ReferenceTarget::Direct(id) => {
                                PreviousReferenceValue::MustResolveTo(*id)
                            }
                            ReferenceTarget::Symbolic(name) => PreviousReferenceValue::MustExist(
                                ReferenceTarget::Symbolic(name.clone()),
                            ),
                        }),
                ))
            }
            UpdateRefCommand::SymbolicCreate { name, new } => Ok(ReferenceTransactionEdit::update(
                name,
                ReferenceTarget::Symbolic(new.clone()),
                PreviousReferenceValue::MustNotExist,
            )),
            UpdateRefCommand::SymbolicDelete { name, old } => Ok(ReferenceTransactionEdit::delete(
                name,
                old.as_ref().map_or(PreviousReferenceValue::Any, |target| {
                    PreviousReferenceValue::MustExist(ReferenceTarget::Symbolic(target.clone()))
                }),
            )),
            UpdateRefCommand::SymbolicVerify { name, old } => Ok(ReferenceTransactionEdit::verify(
                name,
                old.as_ref()
                    .map_or(PreviousReferenceValue::MustNotExist, |target| {
                        PreviousReferenceValue::MustExist(ReferenceTarget::Symbolic(target.clone()))
                    }),
            )),
        }
    }

    fn update_ref_destination(&self, name: &str, no_deref: bool) -> Result<String> {
        if no_deref {
            return Ok(name.to_owned());
        }
        match self.symbolic_reference(name, true) {
            Ok(target) => Ok(target.as_str().to_owned()),
            Err(Error::InvalidReference(_) | Error::NotFound(_)) => Ok(name.to_owned()),
            Err(error) => Err(error),
        }
    }

    fn validate_update_ref_target(
        &self,
        name: &str,
        id: ObjectId,
        max_object_size: usize,
    ) -> Result<()> {
        let object = self.read_object(id, max_object_size)?;
        if name.starts_with("refs/heads/") && object.kind() != crate::ObjectKind::Commit {
            return update_error("branch reference target is not a commit");
        }
        Ok(())
    }
}

fn update_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidReference(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{UpdateRefCommand, UpdateRefOptions};
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, ObjectId, ReferenceName, ReferenceTarget,
        Repository, Signature, Tree,
    };

    #[test]
    fn atomically_mixes_dereferenced_direct_and_symbolic_commands() {
        let repository = repository();
        let first = commit(&repository, 1);
        let second = commit(&repository, 2);
        let main = ReferenceName::branch("main").unwrap();
        let topic = ReferenceName::branch("topic").unwrap();
        repository
            .update_refs(
                &[
                    UpdateRefCommand::Create {
                        name: main.to_string(),
                        new: first,
                        no_deref: false,
                    },
                    UpdateRefCommand::SymbolicCreate {
                        name: "refs/meta/orig".into(),
                        new: main.clone(),
                    },
                ],
                &UpdateRefOptions::default(),
            )
            .unwrap();
        repository
            .update_refs(
                &[
                    UpdateRefCommand::Update {
                        name: "HEAD".into(),
                        new: second,
                        old: Some(first),
                        no_deref: false,
                    },
                    UpdateRefCommand::SymbolicUpdate {
                        name: "refs/meta/orig".into(),
                        new: topic,
                        old: Some(ReferenceTarget::Symbolic(main.clone())),
                    },
                ],
                &UpdateRefOptions::default(),
            )
            .unwrap();
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), second);
        assert_eq!(
            repository
                .read_reference("refs/meta/orig")
                .unwrap()
                .target(),
            &ReferenceTarget::Symbolic(ReferenceName::branch("topic").unwrap())
        );
    }

    #[test]
    fn stale_or_missing_object_commands_leave_every_ref_unchanged() {
        let repository = repository();
        let first = commit(&repository, 1);
        let second = commit(&repository, 2);
        let main = ReferenceName::branch("main").unwrap();
        repository
            .update_refs(
                &[UpdateRefCommand::Create {
                    name: main.to_string(),
                    new: first,
                    no_deref: false,
                }],
                &UpdateRefOptions::default(),
            )
            .unwrap();
        assert!(
            repository
                .update_refs(
                    &[
                        UpdateRefCommand::Update {
                            name: main.to_string(),
                            new: second,
                            old: Some(second),
                            no_deref: false,
                        },
                        UpdateRefCommand::Create {
                            name: "refs/heads/other".into(),
                            new: second,
                            no_deref: false,
                        },
                    ],
                    &UpdateRefOptions::default()
                )
                .is_err()
        );
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), first);
        assert!(repository.read_reference("refs/heads/other").is_err());
    }

    #[test]
    fn updates_root_pseudorefs_and_rejects_non_commit_branch_targets() {
        let repository = repository();
        let commit = commit(&repository, 1);
        repository
            .update_refs(
                &[UpdateRefCommand::Create {
                    name: "ORIG_HEAD".into(),
                    new: commit,
                    no_deref: true,
                }],
                &UpdateRefOptions::default(),
            )
            .unwrap();
        assert_eq!(repository.resolve_reference("ORIG_HEAD").unwrap(), commit);
        let blob = repository
            .write_object(crate::ObjectKind::Blob, b"not a commit")
            .unwrap();
        assert!(
            repository
                .update_refs(
                    &[UpdateRefCommand::Create {
                        name: "refs/heads/invalid".into(),
                        new: blob,
                        no_deref: false,
                    }],
                    &UpdateRefOptions::default()
                )
                .is_err()
        );
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), ".", &InitOptions::default()).unwrap()
    }

    fn commit(repository: &Repository, timestamp: i64) -> ObjectId {
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("A", "a@example.com", timestamp, 0).unwrap();
        repository
            .write_commit(&CommitBuilder::new(tree, identity.clone(), identity).build())
            .unwrap()
    }
}
