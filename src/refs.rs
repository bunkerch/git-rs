//! Git references and reference transactions.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{Error, ObjectId, Repository, Result, Signature};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviousReferenceValue {
    Any,
    MustNotExist,
    MustExist(ReferenceTarget),
    MustResolveTo(ObjectId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceTransactionChange {
    Update(ReferenceTarget),
    Delete,
    Verify,
}

/// One direct or symbolic edit in a mixed atomic reference transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceTransactionEdit {
    name: String,
    change: ReferenceTransactionChange,
    previous: PreviousReferenceValue,
}

impl ReferenceTransactionEdit {
    #[must_use]
    pub fn update(
        name: impl Into<String>,
        target: ReferenceTarget,
        previous: PreviousReferenceValue,
    ) -> Self {
        Self {
            name: name.into(),
            change: ReferenceTransactionChange::Update(target),
            previous,
        }
    }

    #[must_use]
    pub fn delete(name: impl Into<String>, previous: PreviousReferenceValue) -> Self {
        Self {
            name: name.into(),
            change: ReferenceTransactionChange::Delete,
            previous,
        }
    }

    #[must_use]
    pub fn verify(name: impl Into<String>, previous: PreviousReferenceValue) -> Self {
        Self {
            name: name.into(),
            change: ReferenceTransactionChange::Verify,
            previous,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn change(&self) -> &ReferenceTransactionChange {
        &self.change
    }

    #[must_use]
    pub const fn previous(&self) -> &PreviousReferenceValue {
        &self.previous
    }
}

/// One direct update or deletion in a batch reference transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceEdit {
    name: ReferenceName,
    new: Option<ObjectId>,
    previous: PreviousValue,
}

impl ReferenceEdit {
    #[must_use]
    pub const fn update(name: ReferenceName, new: ObjectId, previous: PreviousValue) -> Self {
        Self {
            name,
            new: Some(new),
            previous,
        }
    }

    #[must_use]
    pub const fn delete(name: ReferenceName, expected: ObjectId) -> Self {
        Self {
            name,
            new: None,
            previous: PreviousValue::MustExist(expected),
        }
    }

    #[must_use]
    pub const fn name(&self) -> &ReferenceName {
        &self.name
    }

    #[must_use]
    pub const fn new_id(&self) -> Option<ObjectId> {
        self.new
    }

    #[must_use]
    pub const fn previous(&self) -> PreviousValue {
        self.previous
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogEntry {
    old: ObjectId,
    new: ObjectId,
    committer: Signature,
    message: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReflogRewriteOptions {
    pub dry_run: bool,
    pub rewrite: bool,
    pub update_reference: bool,
    pub max_entries: usize,
    pub max_reference_depth: usize,
    pub max_stale_objects: usize,
}

impl Default for ReflogRewriteOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            rewrite: false,
            update_reference: false,
            max_entries: 10_000_000,
            max_reference_depth: 4096,
            max_stale_objects: 10_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReflogRewriteResult {
    pub removed: usize,
    pub retained: usize,
    pub new_tip: Option<ObjectId>,
}

impl ReflogEntry {
    #[must_use]
    pub const fn old_id(&self) -> ObjectId {
        self.old
    }

    #[must_use]
    pub const fn new_id(&self) -> ObjectId {
        self.new
    }

    #[must_use]
    pub const fn committer(&self) -> &Signature {
        &self.committer
    }

    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }
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
        self.references_with_prefix_bounded("refs/heads/", usize::MAX, usize::MAX)
    }

    /// List every loose and packed reference below `refs/` in bytewise order.
    ///
    /// Loose references override packed references with the same name. Lock
    /// files are excluded and symbolic references are preserved.
    ///
    /// # Errors
    /// Returns an error for malformed references or storage failures.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    pub fn references(&self) -> Result<Vec<Reference>> {
        self.references_with_prefix_bounded("refs/", usize::MAX, usize::MAX)
    }

    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    pub(crate) fn references_with_prefix_bounded(
        &self,
        prefix: &str,
        max_references: usize,
        max_depth: usize,
    ) -> Result<Vec<Reference>> {
        let mut references = self.packed_references_with_prefix_bounded(prefix, max_references)?;
        if references.len() > max_references {
            return Err(Error::InvalidRepository(
                "reference enumeration exceeds limit".into(),
            ));
        }
        let root = self.git_path("refs");
        let mut directories = vec![(root, String::from("refs"), 0usize)];
        while let Some((directory, directory_name, depth)) = directories.pop() {
            if depth > max_depth {
                return Err(Error::InvalidRepository(
                    "reference enumeration exceeds depth limit".into(),
                ));
            }
            for child in self.filesystem().read_dir(&directory)? {
                let path = directory.join(&child);
                let name = format!("{directory_name}/{}", child.to_string_lossy());
                let metadata = self.filesystem().metadata(&path)?;
                if metadata.is_dir() {
                    let directory_prefix = format!("{name}/");
                    if prefix.starts_with(&directory_prefix) || name.starts_with(prefix) {
                        directories.push((path, name, depth.saturating_add(1)));
                    }
                } else if metadata.is_file() && !name.ends_with(".lock") && name.starts_with(prefix)
                {
                    references.insert(name.clone(), self.read_loose_reference(&name)?);
                    if references.len() > max_references {
                        return Err(Error::InvalidRepository(
                            "reference enumeration exceeds limit".into(),
                        ));
                    }
                }
            }
        }
        Ok(references.into_values().collect())
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

    /// Return the target name of a symbolic reference.
    ///
    /// With `recurse`, symbolic chains are followed and the final symbolic
    /// target is returned, including when that target is unborn.
    ///
    /// # Errors
    /// Returns an error when `name` is missing, direct, malformed, or exceeds
    /// Git's five-hop symbolic-reference limit.
    pub fn symbolic_reference(&self, name: &str, recurse: bool) -> Result<ReferenceName> {
        validate_read_name(name)?;
        let mut current = name.to_owned();
        for _ in 0..SYMBOLIC_REF_MAX_DEPTH {
            let reference = self.read_reference(&current)?;
            let ReferenceTarget::Symbolic(target) = reference.target else {
                return Err(Error::InvalidReference(format!(
                    "{name} is not a symbolic reference"
                )));
            };
            if !recurse {
                return Ok(target);
            }
            current.clone_from(&target.0);
            match self.read_reference(&current) {
                Ok(next) if matches!(next.target, ReferenceTarget::Symbolic(_)) => {}
                Ok(_) | Err(Error::NotFound(_)) => return Ok(target),
                Err(error) => return Err(error),
            }
        }
        Err(Error::SymbolicReferenceLoop(name.to_owned()))
    }

    /// Atomically create or replace a symbolic reference without dereferencing it.
    ///
    /// If `reflog` is supplied, its lock is acquired before checking the CAS
    /// precondition and the old/new resolved IDs are appended atomically with
    /// the reference update. Unborn targets are represented by the null ID.
    ///
    /// # Errors
    /// Returns an error for invalid names/messages, a stale previous value,
    /// lock contention, malformed refs, or storage failures.
    pub fn update_symbolic_reference(
        &self,
        name: &str,
        target: &ReferenceName,
        previous: PreviousReferenceValue,
        reflog: Option<(&Signature, &[u8])>,
    ) -> Result<()> {
        validate_read_name(name)?;
        if let Some((_, message)) = reflog {
            validate_reflog_message(message)?;
        }
        let destination = self.git_path(name);
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let lock = lock_path(&destination);
        self.filesystem().write_new(&lock, b"")?;
        let log_destination = self.git_path(Path::new("logs").join(name));
        let log_lock = lock_path(&log_destination);
        if reflog.is_some() {
            if let Some(parent) = log_destination.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            if let Err(error) = self.filesystem().write_new(&log_lock, b"") {
                let _ = self.filesystem().remove_file(&lock);
                return Err(error);
            }
        }

        let result = (|| {
            let actual = match self.read_reference(name) {
                Ok(reference) => Some(reference.target),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            let matches = match previous {
                PreviousReferenceValue::Any => true,
                PreviousReferenceValue::MustNotExist => actual.is_none(),
                PreviousReferenceValue::MustExist(expected) => actual == Some(expected),
                PreviousReferenceValue::MustResolveTo(expected) => {
                    actual.is_some() && resolved_target(self, actual.as_ref()) == expected
                }
            };
            if !matches {
                return Err(Error::ReferenceConflict(name.to_owned()));
            }
            if let Some((committer, message)) = reflog {
                let old = resolved_target(self, actual.as_ref());
                let new = self
                    .resolve_reference(target.as_str())
                    .unwrap_or_else(|_| ObjectId::null());
                let mut contents = match self.filesystem().read(&log_destination) {
                    Ok(contents) => contents,
                    Err(Error::NotFound(_)) => Vec::new(),
                    Err(error) => return Err(error),
                };
                append_reflog_line(&mut contents, old, new, committer, message);
                self.filesystem().write(&log_lock, &contents)?;
            }
            let contents = format!("ref: {target}\n");
            self.filesystem().write(&lock, contents.as_bytes())?;
            if reflog.is_some() {
                self.filesystem().rename(&log_lock, &log_destination)?;
            }
            self.filesystem().rename(&lock, &destination)
        })();
        if result.is_err() {
            let _ = self.filesystem().remove_file(&lock);
            if reflog.is_some() {
                let _ = self.filesystem().remove_file(&log_lock);
            }
        }
        result
    }

    /// Delete a symbolic ref without dereferencing its target.
    ///
    /// `HEAD` cannot be deleted. The expected target provides compare-and-swap
    /// protection, and a successful deletion also removes the ref's log.
    ///
    /// # Errors
    /// Returns an error for `HEAD`, a missing/direct/stale ref, lock contention,
    /// malformed storage, or filesystem failures.
    pub fn delete_symbolic_reference(&self, name: &str, expected: &ReferenceName) -> Result<()> {
        validate_read_name(name)?;
        if name == "HEAD" {
            return Err(Error::InvalidReference(
                "deleting HEAD is not allowed".into(),
            ));
        }
        let destination = self.git_path(name);
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let lock = lock_path(&destination);
        self.filesystem().write_new(&lock, b"")?;
        let result = (|| {
            let reference = self.read_reference(name)?;
            if reference.target != ReferenceTarget::Symbolic(expected.clone()) {
                return Err(Error::ReferenceConflict(name.to_owned()));
            }
            self.filesystem().remove_file(&destination)?;
            self.filesystem().remove_file(&lock)?;
            let log = self.git_path(Path::new("logs").join(name));
            match self.filesystem().remove_file(&log) {
                Ok(()) | Err(Error::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        })();
        if result.is_err() {
            let _ = self.filesystem().remove_file(&lock);
        }
        result
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
        self.update_reference_inner(name, new, previous, None)
    }

    /// Delete a loose or packed reference with a compare-and-swap precondition.
    ///
    /// The loose ref and `packed-refs` locks are held while the current value is
    /// checked. Removing both forms prevents a packed value hidden by a loose
    /// override from reappearing after deletion.
    ///
    /// # Errors
    /// Returns an error for a missing or stale ref, lock contention, symbolic
    /// refs, malformed packed refs, or storage failures.
    pub fn delete_reference(&self, name: &ReferenceName, expected: ObjectId) -> Result<()> {
        if expected.is_null() {
            return Err(Error::InvalidReference(
                "delete precondition cannot be the null object ID".into(),
            ));
        }
        let destination = self.git_path(name.as_str());
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let loose_lock = lock_path(&destination);
        self.filesystem().write_new(&loose_lock, b"")?;
        let packed_path = self.git_path("packed-refs");
        let packed_lock = lock_path(&packed_path);
        if let Err(error) = self.filesystem().write_new(&packed_lock, b"") {
            let _ = self.filesystem().remove_file(&loose_lock);
            return Err(error);
        }

        let result = (|| {
            let actual = self.read_reference(name.as_str())?;
            match actual.target {
                ReferenceTarget::Direct(id) if id == expected => {}
                ReferenceTarget::Direct(_) | ReferenceTarget::Symbolic(_) => {
                    return Err(Error::ReferenceConflict(name.0.clone()));
                }
            }
            let packed = match self.filesystem().read(&packed_path) {
                Ok(contents) => Some(contents),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            if let Some(contents) = packed {
                let filtered = remove_packed_reference(&contents, name.as_str())?;
                self.filesystem().write(&packed_lock, &filtered)?;
                self.filesystem().rename(&packed_lock, &packed_path)?;
            } else {
                self.filesystem().remove_file(&packed_lock)?;
            }
            match self.filesystem().remove_file(&destination) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            self.filesystem().remove_file(&loose_lock)?;

            let log = self.git_path(Path::new("logs").join(name.as_str()));
            match self.filesystem().remove_file(&log) {
                Ok(()) | Err(Error::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        })();
        if result.is_err() {
            let _ = self.filesystem().remove_file(&loose_lock);
            let _ = self.filesystem().remove_file(&packed_lock);
        }
        result?;
        if name.as_str().starts_with("refs/replace/") {
            self.invalidate_replacements()?;
        }
        Ok(())
    }

    /// Apply several direct ref updates/deletions as one prepared transaction.
    ///
    /// All canonical loose locks are acquired in bytewise refname order before
    /// any precondition is evaluated. If a precondition or preparation step
    /// fails, every lock is removed and no ref is changed. Packed deletions are
    /// prepared under `packed-refs.lock` before publication begins.
    ///
    /// # Errors
    /// Returns an error for duplicate or case-conflicting edits, invalid null
    /// updates, stale values, symbolic refs, lock contention, directory/file
    /// conflicts, malformed packed refs, or storage failures.
    pub fn apply_reference_transaction(&self, edits: &[ReferenceEdit]) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        let mut edits = edits.to_vec();
        edits.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        if edits.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(Error::InvalidReference(
                "duplicate ref in transaction".into(),
            ));
        }
        if has_case_conflicting_updates(edits.iter().map(|edit| edit.name.as_str())) {
            return Err(Error::InvalidReference(
                "case-conflicting refs in transaction".into(),
            ));
        }
        if edits
            .iter()
            .any(|edit| edit.new.is_some_and(|id| id.is_null()))
        {
            return Err(Error::InvalidReference(
                "a ref cannot point to the null object ID".into(),
            ));
        }

        let mut prepared = Vec::with_capacity(edits.len());
        for edit in edits {
            let destination = self.git_path(edit.name.as_str());
            if let Some(parent) = destination.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            if self
                .filesystem()
                .metadata(&destination)
                .is_ok_and(crate::Metadata::is_dir)
            {
                cleanup_ref_locks(self, &prepared);
                return Err(Error::ReferenceConflict(edit.name.0));
            }
            let lock = lock_path(&destination);
            if let Err(error) = self.filesystem().write_new(&lock, b"") {
                cleanup_ref_locks(self, &prepared);
                return Err(error);
            }
            prepared.push(PreparedReferenceEdit {
                edit,
                destination,
                lock,
            });
        }

        let deletes_packed = prepared.iter().any(|item| item.edit.new.is_none());
        let packed_path = self.git_path("packed-refs");
        let packed_lock = lock_path(&packed_path);
        if deletes_packed && let Err(error) = self.filesystem().write_new(&packed_lock, b"") {
            cleanup_ref_locks(self, &prepared);
            return Err(error);
        }

        let preparation =
            self.prepare_reference_edits(&prepared, deletes_packed, &packed_path, &packed_lock);

        let packed_exists = match preparation {
            Ok(value) => value,
            Err(error) => {
                cleanup_ref_locks(self, &prepared);
                if deletes_packed {
                    let _ = self.filesystem().remove_file(&packed_lock);
                }
                return Err(error);
            }
        };

        if deletes_packed {
            if packed_exists {
                self.filesystem().rename(&packed_lock, &packed_path)?;
            } else {
                self.filesystem().remove_file(&packed_lock)?;
            }
        }
        let touches_replacements = prepared
            .iter()
            .any(|item| item.edit.name.as_str().starts_with("refs/replace/"));
        for item in &prepared {
            if item.edit.new.is_some() {
                self.filesystem().rename(&item.lock, &item.destination)?;
            } else {
                match self.filesystem().remove_file(&item.destination) {
                    Ok(()) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
                self.filesystem().remove_file(&item.lock)?;
                let log = self.git_path(Path::new("logs").join(item.edit.name.as_str()));
                match self.filesystem().remove_file(&log) {
                    Ok(()) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if touches_replacements {
            self.invalidate_replacements()?;
        }
        Ok(())
    }

    /// Apply direct and symbolic updates, deletions, and verifications using one
    /// prepared set of loose-reference locks.
    ///
    /// Packed deletions are prepared under `packed-refs.lock`. Every
    /// precondition is checked only after all locks have been acquired, and a
    /// preparation failure publishes no change.
    ///
    /// # Errors
    /// Returns an error for invalid, duplicate, or case-conflicting names,
    /// null direct targets, stale target preconditions, absent deletes, lock
    /// contention, malformed packed refs, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn apply_mixed_reference_transaction(
        &self,
        edits: &[ReferenceTransactionEdit],
    ) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        let mut edits = edits.to_vec();
        edits.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        if edits.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(Error::InvalidReference(
                "duplicate ref in transaction".into(),
            ));
        }
        if has_case_conflicting_updates(edits.iter().map(|edit| edit.name.as_str())) {
            return Err(Error::InvalidReference(
                "case-conflicting refs in transaction".into(),
            ));
        }
        for edit in &edits {
            validate_read_name(&edit.name)?;
            if matches!(edit.change, ReferenceTransactionChange::Update(ReferenceTarget::Direct(id)) if id.is_null())
            {
                return Err(Error::InvalidReference(
                    "a ref cannot point to the null object ID".into(),
                ));
            }
        }

        let mut prepared = Vec::with_capacity(edits.len());
        for edit in edits {
            let destination = self.git_path(&edit.name);
            if let Some(parent) = destination.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            if self
                .filesystem()
                .metadata(&destination)
                .is_ok_and(crate::Metadata::is_dir)
            {
                cleanup_mixed_ref_locks(self, &prepared);
                return Err(Error::ReferenceConflict(edit.name));
            }
            let lock = lock_path(&destination);
            if let Err(error) = self.filesystem().write_new(&lock, b"") {
                cleanup_mixed_ref_locks(self, &prepared);
                return Err(error);
            }
            prepared.push(PreparedMixedReferenceEdit {
                edit,
                destination,
                lock,
            });
        }

        let deletes = prepared
            .iter()
            .any(|item| matches!(item.edit.change, ReferenceTransactionChange::Delete));
        let packed_path = self.git_path("packed-refs");
        let packed_lock = lock_path(&packed_path);
        if deletes && let Err(error) = self.filesystem().write_new(&packed_lock, b"") {
            cleanup_mixed_ref_locks(self, &prepared);
            return Err(error);
        }
        let preparation =
            self.prepare_mixed_reference_edits(&prepared, deletes, &packed_path, &packed_lock);
        let packed_exists = match preparation {
            Ok(value) => value,
            Err(error) => {
                cleanup_mixed_ref_locks(self, &prepared);
                if deletes {
                    let _ = self.filesystem().remove_file(&packed_lock);
                }
                return Err(error);
            }
        };
        if deletes {
            if packed_exists {
                self.filesystem().rename(&packed_lock, &packed_path)?;
            } else {
                self.filesystem().remove_file(&packed_lock)?;
            }
        }
        let touches_replacements = prepared
            .iter()
            .any(|item| item.edit.name.starts_with("refs/replace/"));
        for item in &prepared {
            match item.edit.change {
                ReferenceTransactionChange::Update(_) => {
                    self.filesystem().rename(&item.lock, &item.destination)?;
                }
                ReferenceTransactionChange::Delete => {
                    match self.filesystem().remove_file(&item.destination) {
                        Ok(()) | Err(Error::NotFound(_)) => {}
                        Err(error) => return Err(error),
                    }
                    self.filesystem().remove_file(&item.lock)?;
                    let log = self.git_path(Path::new("logs").join(&item.edit.name));
                    match self.filesystem().remove_file(&log) {
                        Ok(()) | Err(Error::NotFound(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
                ReferenceTransactionChange::Verify => {
                    self.filesystem().remove_file(&item.lock)?;
                }
            }
        }
        if touches_replacements {
            self.invalidate_replacements()?;
        }
        Ok(())
    }

    fn prepare_mixed_reference_edits(
        &self,
        prepared: &[PreparedMixedReferenceEdit],
        deletes: bool,
        packed_path: &Path,
        packed_lock: &Path,
    ) -> Result<bool> {
        for item in prepared {
            let actual = match self.read_reference(&item.edit.name) {
                Ok(reference) => Some(reference.target),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            if !previous_reference_matches(self, &item.edit.previous, actual.as_ref())
                || (matches!(item.edit.change, ReferenceTransactionChange::Delete)
                    && actual.is_none())
            {
                return Err(Error::ReferenceConflict(item.edit.name.clone()));
            }
            if let ReferenceTransactionChange::Update(ref target) = item.edit.change {
                let contents = match target {
                    ReferenceTarget::Direct(id) => format!("{id}\n"),
                    ReferenceTarget::Symbolic(name) => format!("ref: {name}\n"),
                };
                self.filesystem().write(&item.lock, contents.as_bytes())?;
            }
        }
        let packed = if deletes {
            match self.filesystem().read(packed_path) {
                Ok(contents) => Some(contents),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let packed = packed
            .map(|mut contents| {
                for item in prepared {
                    if matches!(item.edit.change, ReferenceTransactionChange::Delete) {
                        contents = remove_packed_reference(&contents, &item.edit.name)?;
                    }
                }
                Ok::<Vec<u8>, Error>(contents)
            })
            .transpose()?;
        if let Some(contents) = &packed {
            self.filesystem().write(packed_lock, contents)?;
        }
        Ok(packed.is_some())
    }

    fn direct_reference_value(&self, name: &str) -> Result<Option<ObjectId>> {
        match self.read_reference(name) {
            Ok(reference) => match reference.target {
                ReferenceTarget::Direct(id) => Ok(Some(id)),
                ReferenceTarget::Symbolic(_) => Err(Error::ReferenceConflict(name.to_owned())),
            },
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn prepare_reference_edits(
        &self,
        prepared: &[PreparedReferenceEdit],
        deletes_packed: bool,
        packed_path: &Path,
        packed_lock: &Path,
    ) -> Result<bool> {
        for item in prepared {
            let actual = self.direct_reference_value(item.edit.name.as_str())?;
            if !previous_matches(item.edit.previous, actual)
                || (item.edit.new.is_none() && actual.is_none())
            {
                return Err(Error::ReferenceConflict(item.edit.name.0.clone()));
            }
            if let Some(new) = item.edit.new {
                let mut contents = new.to_hex().to_vec();
                contents.push(b'\n');
                self.filesystem().write(&item.lock, &contents)?;
            }
        }

        let packed = if deletes_packed {
            match self.filesystem().read(packed_path) {
                Ok(contents) => Some(contents),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let packed = packed
            .map(|mut contents| {
                for item in prepared {
                    if item.edit.new.is_none() {
                        contents = remove_packed_reference(&contents, item.edit.name.as_str())?;
                    }
                }
                Ok::<Vec<u8>, Error>(contents)
            })
            .transpose()?;
        if let Some(contents) = &packed {
            self.filesystem().write(packed_lock, contents)?;
        }
        Ok(packed.is_some())
    }

    /// Atomically update a direct reference and append its reflog.
    ///
    /// # Errors
    /// Returns an error for invalid log messages, stale previous values, lock
    /// contention, malformed current refs, or storage failures.
    pub fn update_reference_with_reflog(
        &self,
        name: &ReferenceName,
        new: ObjectId,
        previous: PreviousValue,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        validate_reflog_message(message)?;
        self.update_reference_inner(name, new, previous, Some((committer, message)))
    }

    fn update_reference_inner(
        &self,
        name: &ReferenceName,
        new: ObjectId,
        previous: PreviousValue,
        reflog: Option<(&Signature, &[u8])>,
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

        let log_destination = self.git_path(Path::new("logs").join(name.as_str()));
        let log_lock = lock_path(&log_destination);
        if reflog.is_some() {
            if let Some(parent) = log_destination.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            if let Err(error) = self.filesystem().write_new(&log_lock, b"") {
                let _ = self.filesystem().remove_file(&lock);
                return Err(error);
            }
        }

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

            if let Some((committer, message)) = reflog {
                let mut contents = match self.filesystem().read(&log_destination) {
                    Ok(contents) => contents,
                    Err(Error::NotFound(_)) => Vec::new(),
                    Err(error) => return Err(error),
                };
                append_reflog_line(
                    &mut contents,
                    actual.unwrap_or_else(ObjectId::null),
                    new,
                    committer,
                    message,
                );
                self.filesystem().write(&log_lock, &contents)?;
            }

            let mut contents = new.to_hex().to_vec();
            contents.push(b'\n');
            self.filesystem().write(&lock, &contents)?;
            if reflog.is_some() {
                self.filesystem().rename(&log_lock, &log_destination)?;
            }
            self.filesystem().rename(&lock, &destination)
        })();

        if result.is_err() {
            let _ = self.filesystem().remove_file(&lock);
            if reflog.is_some() {
                let _ = self.filesystem().remove_file(&log_lock);
            }
        }
        result?;
        if name.as_str().starts_with("refs/replace/") {
            self.invalidate_replacements()?;
        }
        Ok(())
    }

    /// Read a reference log from oldest to newest.
    ///
    /// # Errors
    /// Returns an error for invalid names, malformed log lines, or storage failures.
    pub fn read_reflog(&self, name: &str) -> Result<Vec<ReflogEntry>> {
        self.read_reflog_bounded(name, usize::MAX)
    }

    /// Read a reference log from oldest to newest with an entry bound.
    ///
    /// # Errors
    /// Returns an error for invalid names, malformed log lines, an exceeded
    /// entry limit, or storage failures.
    pub fn read_reflog_bounded(&self, name: &str, max_entries: usize) -> Result<Vec<ReflogEntry>> {
        validate_read_name(name)?;
        let contents = match self
            .filesystem()
            .read(&self.git_path(Path::new("logs").join(name)))
        {
            Ok(contents) => contents,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut entries = Vec::new();
        for line in contents
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            entries.push(parse_reflog_line(line)?);
            if entries.len() > max_entries {
                return Err(Error::InvalidRepository(
                    "reflog exceeds entry limit".into(),
                ));
            }
        }
        Ok(entries)
    }

    /// List reflogs in bytewise reference-name order.
    ///
    /// # Errors
    /// Returns an error for malformed/non-UTF-8 names, exceeded count/depth
    /// limits, or storage failures.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    pub fn reflogs(&self, max_reflogs: usize, max_depth: usize) -> Result<Vec<String>> {
        let root = self.git_path("logs");
        match self.filesystem().metadata(&root) {
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        let mut pending = vec![(root, String::new(), 0usize)];
        let mut output = Vec::new();
        while let Some((directory, prefix, depth)) = pending.pop() {
            if depth > max_depth {
                return Err(Error::InvalidRepository(
                    "reflog enumeration exceeds depth limit".into(),
                ));
            }
            for child in self.filesystem().read_dir(&directory)? {
                let component = child
                    .to_str()
                    .ok_or_else(|| Error::InvalidReference("non-UTF-8 reflog name".into()))?;
                let name = if prefix.is_empty() {
                    component.to_owned()
                } else {
                    format!("{prefix}/{component}")
                };
                let path = directory.join(child);
                let metadata = self.filesystem().metadata(&path)?;
                if metadata.is_dir() {
                    pending.push((path, name, depth.saturating_add(1)));
                } else if metadata.is_file() && !name.ends_with(".lock") {
                    validate_read_name(&name)?;
                    output.push(name);
                    if output.len() > max_reflogs {
                        return Err(Error::InvalidRepository(
                            "reflog enumeration exceeds limit".into(),
                        ));
                    }
                }
            }
        }
        output.sort_unstable();
        Ok(output)
    }

    /// Remove one complete reflog while holding its canonical log lock.
    ///
    /// Returns `false` when no log exists.
    ///
    /// # Errors
    /// Returns an error for an invalid name, lock contention, or storage failure.
    pub fn drop_reflog(&self, name: &str) -> Result<bool> {
        validate_read_name(name)?;
        let destination = self.git_path(Path::new("logs").join(name));
        if !self.filesystem().exists(&destination)? {
            return Ok(false);
        }
        let lock = lock_path(&destination);
        self.filesystem().write_new(&lock, b"")?;
        let result = self.filesystem().remove_file(&destination);
        let _ = self.filesystem().remove_file(&lock);
        result.map(|()| true)
    }

    /// Delete entries addressed by zero-based positions from the newest entry.
    ///
    /// # Errors
    /// Returns an error for duplicate/out-of-range selectors, malformed state,
    /// lock contention, a symbolic update target, or storage failures.
    pub fn delete_reflog_entries(
        &self,
        name: &str,
        newest_indices: &[usize],
        options: &ReflogRewriteOptions,
    ) -> Result<ReflogRewriteResult> {
        let mut selected = std::collections::BTreeSet::new();
        if newest_indices.iter().any(|index| !selected.insert(*index)) {
            return Err(Error::InvalidReference(
                "duplicate reflog deletion selector".into(),
            ));
        }
        self.rewrite_reflog(
            name,
            options,
            |index, len, _| Ok(selected.contains(&(len - index - 1))),
            Some(selected.len()),
        )
    }

    /// Expire every reflog entry strictly older than `timestamp`.
    ///
    /// # Errors
    /// Returns an error for malformed state, lock contention, a symbolic update
    /// target, exceeded limits, or storage failures.
    pub fn expire_reflog_before(
        &self,
        name: &str,
        timestamp: i64,
        options: &ReflogRewriteOptions,
    ) -> Result<ReflogRewriteResult> {
        self.rewrite_reflog(
            name,
            options,
            |_, _, entry| Ok(entry.committer.timestamp() < timestamp),
            None,
        )
    }

    /// Expire old entries whose old or new commit is unreachable from the
    /// current reference tip. For `HEAD`, all reference tips are roots.
    ///
    /// Non-commit object IDs are retained, matching Git's gentle commit lookup;
    /// when a non-commit direct reference is the root, all old entries expire.
    ///
    /// # Errors
    /// Returns an error for malformed references/objects, graph or storage
    /// failures, exceeded limits, or lock contention.
    pub fn expire_reflog_unreachable_before(
        &self,
        name: &str,
        timestamp: i64,
        graph: &crate::GraphOptions,
        options: &ReflogRewriteOptions,
    ) -> Result<ReflogRewriteResult> {
        let (reachable, expire_all) = self.reflog_reachable_commits(name, graph, options)?;
        self.rewrite_reflog(
            name,
            options,
            |_, _, entry| {
                if entry.committer.timestamp() >= timestamp {
                    return Ok(false);
                }
                if expire_all {
                    return Ok(true);
                }
                Ok(self.reflog_commit_is_unreachable(
                    entry.old,
                    &reachable,
                    graph.max_object_size,
                )? || self.reflog_commit_is_unreachable(
                    entry.new,
                    &reachable,
                    graph.max_object_size,
                )?)
            },
            None,
        )
    }

    /// Expire the union of total-age and unreachable-age reflog policies in
    /// one rewrite, so entries matching both policies are removed once.
    ///
    /// # Errors
    /// Returns an error for malformed references/objects, graph or storage
    /// failures, exceeded limits, or lock contention.
    pub fn expire_reflog_with_policy(
        &self,
        name: &str,
        total_before: Option<i64>,
        unreachable_before: Option<i64>,
        graph: &crate::GraphOptions,
        options: &ReflogRewriteOptions,
    ) -> Result<ReflogRewriteResult> {
        let reachability = unreachable_before
            .map(|_| self.reflog_reachable_commits(name, graph, options))
            .transpose()?;
        self.rewrite_reflog(
            name,
            options,
            |_, _, entry| {
                if total_before.is_some_and(|expiry| entry.committer.timestamp() < expiry) {
                    return Ok(true);
                }
                let Some(expiry) = unreachable_before else {
                    return Ok(false);
                };
                if entry.committer.timestamp() >= expiry {
                    return Ok(false);
                }
                let Some((reachable, expire_all)) = reachability.as_ref() else {
                    return Ok(false);
                };
                if *expire_all {
                    return Ok(true);
                }
                Ok(self.reflog_commit_is_unreachable(
                    entry.old,
                    reachable,
                    graph.max_object_size,
                )? || self.reflog_commit_is_unreachable(
                    entry.new,
                    reachable,
                    graph.max_object_size,
                )?)
            },
            None,
        )
    }

    /// Remove entries whose old or new commit has an incomplete object closure.
    ///
    /// Null endpoints are valid. Non-commit endpoints, missing/corrupt parents,
    /// trees, or blobs make an entry stale. Successfully verified objects are
    /// cached across entries.
    ///
    /// # Errors
    /// Returns an error for storage failures, object size/count limits, malformed
    /// reference state, or lock contention.
    pub fn prune_stale_reflog_entries(
        &self,
        name: &str,
        graph: &crate::GraphOptions,
        options: &ReflogRewriteOptions,
    ) -> Result<ReflogRewriteResult> {
        let mut verified = std::collections::HashSet::new();
        self.rewrite_reflog(
            name,
            options,
            |_, _, entry| {
                Ok(!self.reflog_commit_closure_complete(
                    entry.old,
                    graph.max_object_size,
                    options.max_stale_objects,
                    &mut verified,
                )? || !self.reflog_commit_closure_complete(
                    entry.new,
                    graph.max_object_size,
                    options.max_stale_objects,
                    &mut verified,
                )?)
            },
            None,
        )
    }

    fn reflog_commit_closure_complete(
        &self,
        root: ObjectId,
        max_object_size: usize,
        max_objects: usize,
        verified: &mut std::collections::HashSet<ObjectId>,
    ) -> Result<bool> {
        if root.is_null() || verified.contains(&root) {
            return Ok(true);
        }
        let mut pending = vec![(root, crate::ObjectKind::Commit)];
        let mut discovered = std::collections::HashSet::new();
        while let Some((id, expected)) = pending.pop() {
            if verified.contains(&id) || !discovered.insert(id) {
                continue;
            }
            if verified.len().saturating_add(discovered.len()) > max_objects {
                return Err(Error::InvalidRepository(
                    "stale reflog verification exceeds object limit".into(),
                ));
            }
            let object = match self.read_object(id, max_object_size) {
                Ok(object) => object,
                Err(error) if reflog_broken_object_error(&error) => return Ok(false),
                Err(error) => return Err(error),
            };
            if object.kind() != expected {
                return Ok(false);
            }
            match expected {
                crate::ObjectKind::Blob => {}
                crate::ObjectKind::Commit => {
                    let commit = match crate::Commit::parse(object.data()) {
                        Ok(commit) => commit,
                        Err(error) if reflog_broken_object_error(&error) => return Ok(false),
                        Err(error) => return Err(error),
                    };
                    pending.push((commit.tree(), crate::ObjectKind::Tree));
                    pending.extend(
                        commit
                            .parents()
                            .iter()
                            .copied()
                            .map(|parent| (parent, crate::ObjectKind::Commit)),
                    );
                }
                crate::ObjectKind::Tree => {
                    let tree = match crate::Tree::parse(object.data()) {
                        Ok(tree) => tree,
                        Err(error) if reflog_broken_object_error(&error) => return Ok(false),
                        Err(error) => return Err(error),
                    };
                    pending.extend(
                        tree.entries()
                            .iter()
                            .filter(|entry| entry.mode() != crate::EntryMode::Gitlink)
                            .map(|entry| (entry.id(), entry.mode().object_kind())),
                    );
                }
                crate::ObjectKind::Tag => return Ok(false),
            }
        }
        verified.extend(discovered);
        Ok(true)
    }

    fn reflog_reachable_commits(
        &self,
        name: &str,
        graph: &crate::GraphOptions,
        options: &ReflogRewriteOptions,
    ) -> Result<(std::collections::HashSet<ObjectId>, bool)> {
        let mut roots = Vec::new();
        if name == "HEAD" {
            for reference in self.references_with_prefix_bounded(
                "refs/",
                options.max_entries,
                options.max_reference_depth,
            )? {
                let id = match reference.target() {
                    ReferenceTarget::Direct(id) => *id,
                    ReferenceTarget::Symbolic(_) => self.resolve_reference(reference.name())?,
                };
                if self.read_object(id, graph.max_object_size)?.kind() == crate::ObjectKind::Commit
                {
                    roots.push(id);
                }
            }
        } else {
            let id = self.resolve_reference(name)?;
            if self.read_object(id, graph.max_object_size)?.kind() != crate::ObjectKind::Commit {
                return Ok((std::collections::HashSet::new(), true));
            }
            roots.push(id);
        }
        if roots.is_empty() {
            return Ok((std::collections::HashSet::new(), false));
        }
        let revisions = self.walk_revisions(
            &roots,
            &[],
            &crate::RevisionWalkOptions {
                graph: graph.clone(),
                ..crate::RevisionWalkOptions::default()
            },
        )?;
        Ok((
            revisions
                .into_iter()
                .map(|revision| revision.id())
                .collect(),
            false,
        ))
    }

    fn reflog_commit_is_unreachable(
        &self,
        id: ObjectId,
        reachable: &std::collections::HashSet<ObjectId>,
        max_object_size: usize,
    ) -> Result<bool> {
        if id.is_null() {
            return Ok(false);
        }
        match self.read_object(id, max_object_size) {
            Ok(object) if object.kind() == crate::ObjectKind::Commit => {
                Ok(!reachable.contains(&id))
            }
            Ok(_) | Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn rewrite_reflog(
        &self,
        name: &str,
        options: &ReflogRewriteOptions,
        mut remove: impl FnMut(usize, usize, &ReflogEntry) -> Result<bool>,
        expected_removals: Option<usize>,
    ) -> Result<ReflogRewriteResult> {
        validate_read_name(name)?;
        let reference_destination = self.git_path(name);
        if let Some(parent) = reference_destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let reference_lock = lock_path(&reference_destination);
        self.filesystem().write_new(&reference_lock, b"")?;
        let log_destination = self.git_path(Path::new("logs").join(name));
        let log_lock = lock_path(&log_destination);
        let result = (|| {
            let reference = self.read_reference(name)?;
            if options.update_reference && matches!(reference.target, ReferenceTarget::Symbolic(_))
            {
                return Err(Error::InvalidReference(
                    "cannot update a symbolic reference from its reflog".into(),
                ));
            }
            let entries = self.read_reflog_bounded(name, options.max_entries)?;
            let mut retained = Vec::with_capacity(entries.len());
            let mut removed = 0;
            let mut last_kept = ObjectId::null();
            for (index, entry) in entries.iter().enumerate() {
                let mut candidate = entry.clone();
                if options.rewrite {
                    candidate.old = last_kept;
                }
                if remove(index, entries.len(), &candidate)? {
                    removed += 1;
                } else {
                    last_kept = candidate.new;
                    retained.push(candidate);
                }
            }
            if expected_removals.is_some_and(|expected| removed != expected) {
                return Err(Error::InvalidReference(
                    "reflog deletion selector is out of range".into(),
                ));
            }
            let new_tip = retained.last().map(|entry| entry.new);
            let outcome = ReflogRewriteResult {
                removed,
                retained: retained.len(),
                new_tip,
            };
            if options.dry_run || removed == 0 {
                return Ok(outcome);
            }
            if let Some(parent) = log_destination.parent() {
                self.filesystem().create_dir_all(parent)?;
            }
            self.filesystem().write_new(&log_lock, b"")?;
            let mut contents = Vec::new();
            for entry in &retained {
                append_reflog_line(
                    &mut contents,
                    entry.old,
                    entry.new,
                    &entry.committer,
                    &entry.message,
                );
            }
            self.filesystem().write(&log_lock, &contents)?;
            if options.update_reference
                && let Some(new_tip) = new_tip
            {
                let mut contents = new_tip.to_hex().to_vec();
                contents.push(b'\n');
                self.filesystem().write(&reference_lock, &contents)?;
            }
            self.filesystem().rename(&log_lock, &log_destination)?;
            if options.update_reference && new_tip.is_some() {
                self.filesystem()
                    .rename(&reference_lock, &reference_destination)?;
            }
            Ok(outcome)
        })();
        let _ = self.filesystem().remove_file(&reference_lock);
        let _ = self.filesystem().remove_file(&log_lock);
        result
    }

    pub(crate) fn append_reflog(
        &self,
        name: &str,
        old: ObjectId,
        new: ObjectId,
        committer: &Signature,
        message: &[u8],
    ) -> Result<()> {
        validate_read_name(name)?;
        validate_reflog_message(message)?;
        let destination = self.git_path(Path::new("logs").join(name));
        if let Some(parent) = destination.parent() {
            self.filesystem().create_dir_all(parent)?;
        }
        let lock = lock_path(&destination);
        self.filesystem().write_new(&lock, b"")?;
        let result = (|| {
            let mut contents = match self.filesystem().read(&destination) {
                Ok(contents) => contents,
                Err(Error::NotFound(_)) => Vec::new(),
                Err(error) => return Err(error),
            };
            append_reflog_line(&mut contents, old, new, committer, message);
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
        self.packed_references_with_prefix_bounded(prefix, usize::MAX)
    }

    fn packed_references_with_prefix_bounded(
        &self,
        prefix: &str,
        max_references: usize,
    ) -> Result<BTreeMap<String, Reference>> {
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
                if references.len() > max_references {
                    return Err(Error::InvalidRepository(
                        "packed reference enumeration exceeds limit".into(),
                    ));
                }
            }
        }
        Ok(references)
    }
}

fn validate_read_name(name: &str) -> Result<()> {
    let root_ref = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if root_ref || (name.starts_with("refs/") && is_valid_refname(name, false)) {
        Ok(())
    } else {
        Err(Error::InvalidReferenceName(name.to_owned()))
    }
}

fn reflog_broken_object_error(error: &Error) -> bool {
    matches!(
        error,
        Error::NotFound(_)
            | Error::InvalidObjectId(_)
            | Error::InvalidObject(_)
            | Error::InvalidTree(_)
            | Error::InvalidCommit(_)
            | Error::Compression(_)
    )
}

struct PreparedReferenceEdit {
    edit: ReferenceEdit,
    destination: PathBuf,
    lock: PathBuf,
}

struct PreparedMixedReferenceEdit {
    edit: ReferenceTransactionEdit,
    destination: PathBuf,
    lock: PathBuf,
}

fn cleanup_ref_locks(repository: &Repository, prepared: &[PreparedReferenceEdit]) {
    for item in prepared {
        let _ = repository.filesystem().remove_file(&item.lock);
    }
}

fn cleanup_mixed_ref_locks(repository: &Repository, prepared: &[PreparedMixedReferenceEdit]) {
    for item in prepared {
        let _ = repository.filesystem().remove_file(&item.lock);
    }
}

/// Detect refnames that collide under ASCII case folding, mirroring Git's
/// `REF_TRANSACTION_ERROR_CASE_CONFLICT` intent: two edits in one transaction
/// cannot name refs that differ only in case, because a case-insensitive
/// filesystem could not store them side by side.
fn has_case_conflicting_updates<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = std::collections::HashSet::new();
    names.any(|name| !seen.insert(name.to_ascii_lowercase()))
}

fn previous_matches(previous: PreviousValue, actual: Option<ObjectId>) -> bool {
    match previous {
        PreviousValue::Any => true,
        PreviousValue::MustNotExist => actual.is_none(),
        PreviousValue::MustExist(expected) => actual == Some(expected),
    }
}

fn previous_reference_matches(
    repository: &Repository,
    previous: &PreviousReferenceValue,
    actual: Option<&ReferenceTarget>,
) -> bool {
    match previous {
        PreviousReferenceValue::Any => true,
        PreviousReferenceValue::MustNotExist => actual.is_none(),
        PreviousReferenceValue::MustExist(expected) => actual == Some(expected),
        PreviousReferenceValue::MustResolveTo(expected) => {
            actual.is_some() && resolved_target(repository, actual) == *expected
        }
    }
}

fn remove_packed_reference(contents: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(contents.len());
    let mut removed = false;
    let mut skip_peeled = false;
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if skip_peeled && body.starts_with(b"^") {
            skip_peeled = false;
            continue;
        }
        skip_peeled = false;
        if body.is_empty() || matches!(body[0], b'#' | b'^') {
            output.extend_from_slice(line);
            continue;
        }
        let separator = body
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::InvalidReference("malformed packed-refs entry".into()))?;
        let packed_name = std::str::from_utf8(&body[separator + 1..])
            .map_err(|_| Error::InvalidReference("non-UTF-8 packed ref name".into()))?;
        ReferenceName::new(packed_name.to_owned())?;
        if packed_name == name {
            removed = true;
            skip_peeled = true;
        } else {
            output.extend_from_slice(line);
        }
    }
    if !removed {
        return Ok(contents.to_vec());
    }
    Ok(output)
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

fn validate_reflog_message(message: &[u8]) -> Result<()> {
    if message.contains(&0) || message.contains(&b'\n') || message.contains(&b'\r') {
        return Err(Error::InvalidReference(
            "reflog message contains NUL or newline".into(),
        ));
    }
    Ok(())
}

fn append_reflog_line(
    output: &mut Vec<u8>,
    old: ObjectId,
    new: ObjectId,
    committer: &Signature,
    message: &[u8],
) {
    output.extend_from_slice(&old.to_hex());
    output.push(b' ');
    output.extend_from_slice(&new.to_hex());
    output.push(b' ');
    output.extend_from_slice(committer.encode().as_bytes());
    output.push(b'\t');
    output.extend_from_slice(message);
    output.push(b'\n');
}

fn resolved_target(repository: &Repository, target: Option<&ReferenceTarget>) -> ObjectId {
    match target {
        Some(ReferenceTarget::Direct(id)) => *id,
        Some(ReferenceTarget::Symbolic(name)) => repository
            .resolve_reference(name.as_str())
            .unwrap_or_else(|_| ObjectId::null()),
        None => ObjectId::null(),
    }
}

fn parse_reflog_line(line: &[u8]) -> Result<ReflogEntry> {
    if line.len() < ObjectId::HEX_LENGTH * 2 + 3 {
        return Err(Error::InvalidReference("truncated reflog line".into()));
    }
    let first_space = line
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| Error::InvalidReference("reflog has no old-ID separator".into()))?;
    let second_space = line[first_space + 1..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|position| first_space + 1 + position)
        .ok_or_else(|| Error::InvalidReference("reflog has no new-ID separator".into()))?;
    let tab = line[second_space + 1..]
        .iter()
        .position(|byte| *byte == b'\t')
        .map(|position| second_space + 1 + position)
        .ok_or_else(|| Error::InvalidReference("reflog has no message separator".into()))?;
    let old = std::str::from_utf8(&line[..first_space])
        .map_err(|_| Error::InvalidReference("non-UTF-8 old reflog ID".into()))?
        .parse()?;
    let new = std::str::from_utf8(&line[first_space + 1..second_space])
        .map_err(|_| Error::InvalidReference("non-UTF-8 new reflog ID".into()))?
        .parse()?;
    Ok(ReflogEntry {
        old,
        new,
        committer: Signature::parse(&line[second_space + 1..tab])?,
        message: line[tab + 1..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{CommitBuilder, FileSystem, GraphOptions, InitOptions, MemoryFileSystem, Tree};

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
            "refs/heads/a b",
            "refs/heads/a~b",
            "refs/heads/a^b",
            "refs/heads/a:b",
            "refs/heads/a[b",
            "refs/heads/a\tb",
            "refs/heads/a.b.",
            "refs/heads/a.lock.lock",
            "@",
        ] {
            assert!(ReferenceName::new(invalid).is_err(), "{invalid}");
        }
        // Single-level refs are valid only with allow_one_level.
        assert!(ReferenceName::new("main").is_err());
        assert!(crate::refs::is_valid_refname("main", true));
        assert!(!crate::refs::is_valid_refname(".hidden", true));
        assert!(!crate::refs::is_valid_refname("a..b", true));
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
    fn creates_reads_logs_and_deletes_symbolic_references() {
        let (repository, fs) = repository();
        let first = ObjectId::from_str(FIRST).unwrap();
        let branch = ReferenceName::branch("main").unwrap();
        repository
            .update_reference(&branch, first, PreviousValue::MustNotExist)
            .unwrap();
        let alias = ReferenceName::new("refs/meta/current").unwrap();
        let signature = Signature::new("A U Thor", "author@example.com", 1, 0).unwrap();
        repository
            .update_symbolic_reference(
                "refs/meta/alias",
                &alias,
                PreviousReferenceValue::MustNotExist,
                None,
            )
            .unwrap();
        repository
            .update_symbolic_reference(
                alias.as_str(),
                &branch,
                PreviousReferenceValue::MustNotExist,
                Some((&signature, b"link")),
            )
            .unwrap();
        assert_eq!(
            repository
                .symbolic_reference("refs/meta/alias", false)
                .unwrap(),
            alias
        );
        assert_eq!(
            repository
                .symbolic_reference("refs/meta/alias", true)
                .unwrap(),
            branch
        );
        assert_eq!(
            repository.resolve_reference("refs/meta/alias").unwrap(),
            first
        );
        assert_eq!(
            repository.read_reflog("refs/meta/current").unwrap().len(),
            1
        );
        assert!(matches!(
            repository.update_symbolic_reference(
                "refs/meta/current",
                &alias,
                PreviousReferenceValue::MustNotExist,
                None,
            ),
            Err(Error::ReferenceConflict(_))
        ));
        repository
            .delete_symbolic_reference("refs/meta/current", &branch)
            .unwrap();
        assert!(!fs.exists(Path::new("repo/.git/refs/meta/current")).unwrap());
        assert!(
            repository
                .read_reflog("refs/meta/current")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn symbolic_reference_supports_unborn_targets_and_protects_head() {
        let (repository, _) = repository();
        let unborn = ReferenceName::branch("unborn").unwrap();
        repository
            .update_symbolic_reference("HEAD", &unborn, PreviousReferenceValue::Any, None)
            .unwrap();
        assert_eq!(repository.symbolic_reference("HEAD", true).unwrap(), unborn);
        assert!(
            repository
                .delete_symbolic_reference("HEAD", &unborn)
                .is_err()
        );
        assert!(
            repository
                .symbolic_reference("refs/heads/missing", true)
                .is_err()
        );
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
    fn deletes_loose_and_hidden_packed_forms_without_resurrection() {
        let (repository, fs) = repository();
        let name = ReferenceName::branch("main").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        fs.write(
            Path::new("repo/.git/packed-refs"),
            format!("# pack-refs with: peeled\n{SECOND} refs/heads/main\n^{FIRST}\n").as_bytes(),
        )
        .unwrap();
        fs.write(
            Path::new("repo/.git/refs/heads/main"),
            format!("{FIRST}\n").as_bytes(),
        )
        .unwrap();
        fs.create_dir_all(Path::new("repo/.git/logs/refs/heads"))
            .unwrap();
        fs.write(Path::new("repo/.git/logs/refs/heads/main"), b"log")
            .unwrap();

        repository.delete_reference(&name, first).unwrap();
        assert!(matches!(
            repository.read_reference(name.as_str()),
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            fs.read(Path::new("repo/.git/packed-refs")).unwrap(),
            b"# pack-refs with: peeled\n"
        );
        assert!(
            !fs.exists(Path::new("repo/.git/logs/refs/heads/main"))
                .unwrap()
        );
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/main.lock"))
                .unwrap()
        );
        assert!(!fs.exists(Path::new("repo/.git/packed-refs.lock")).unwrap());
    }

    #[test]
    fn deletes_a_packed_only_ref_and_rejects_stale_values() {
        let (repository, fs) = repository();
        let name = ReferenceName::new("refs/tags/v1").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        fs.write(
            Path::new("repo/.git/packed-refs"),
            format!("{FIRST} refs/tags/v1\n{SECOND} refs/tags/v2\n").as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            repository.delete_reference(&name, second),
            Err(Error::ReferenceConflict(_))
        ));
        repository.delete_reference(&name, first).unwrap();
        assert_eq!(
            fs.read(Path::new("repo/.git/packed-refs")).unwrap(),
            format!("{SECOND} refs/tags/v2\n").as_bytes()
        );
    }

    #[test]
    fn batch_transaction_prepares_all_values_before_changing_any_ref() {
        let (repository, fs) = repository();
        let main = ReferenceName::branch("main").unwrap();
        let topic = ReferenceName::branch("topic").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        repository
            .update_reference(&main, first, PreviousValue::MustNotExist)
            .unwrap();
        repository
            .update_reference(&topic, first, PreviousValue::MustNotExist)
            .unwrap();

        let result = repository.apply_reference_transaction(&[
            ReferenceEdit::update(main.clone(), second, PreviousValue::MustExist(first)),
            ReferenceEdit::update(topic.clone(), second, PreviousValue::MustExist(second)),
        ]);
        assert!(matches!(result, Err(Error::ReferenceConflict(_))));
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), first);
        assert_eq!(repository.resolve_reference(topic.as_str()).unwrap(), first);
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/main.lock"))
                .unwrap()
        );
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/topic.lock"))
                .unwrap()
        );
    }

    #[test]
    fn batch_transaction_updates_and_deletes_packed_refs_together() {
        let (repository, fs) = repository();
        let main = ReferenceName::branch("main").unwrap();
        let obsolete = ReferenceName::branch("obsolete").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        fs.write(
            Path::new("repo/.git/packed-refs"),
            format!("{FIRST} refs/heads/main\n{FIRST} refs/heads/obsolete\n").as_bytes(),
        )
        .unwrap();
        repository
            .apply_reference_transaction(&[
                ReferenceEdit::update(main.clone(), second, PreviousValue::MustExist(first)),
                ReferenceEdit::delete(obsolete.clone(), first),
            ])
            .unwrap();
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), second);
        assert!(repository.resolve_reference(obsolete.as_str()).is_err());
        assert_eq!(
            fs.read(Path::new("repo/.git/packed-refs")).unwrap(),
            format!("{FIRST} refs/heads/main\n").as_bytes()
        );
    }

    #[test]
    fn transaction_rejects_case_conflicting_refnames() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo",
            &InitOptions::default(),
        )
        .unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let result = repository.apply_reference_transaction(&[
            ReferenceEdit::update(
                ReferenceName::branch("main").unwrap(),
                first,
                PreviousValue::MustNotExist,
            ),
            ReferenceEdit::update(
                ReferenceName::branch("Main").unwrap(),
                first,
                PreviousValue::MustNotExist,
            ),
        ]);
        assert!(matches!(result, Err(Error::InvalidReference(_))));
        assert!(repository.resolve_reference("refs/heads/main").is_err());
        assert!(repository.resolve_reference("refs/heads/Main").is_err());
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

    #[test]
    fn writes_and_parses_git_reflog_lines_under_the_ref_lock() {
        let (repository, fs) = repository();
        let name = ReferenceName::branch("main").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        let committer = Signature::with_unknown_timezone("Test", "test@example.com", 123).unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                first,
                PreviousValue::MustNotExist,
                &committer,
                b"commit (initial): base",
            )
            .unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                second,
                PreviousValue::MustExist(first),
                &committer,
                b"commit: second",
            )
            .unwrap();
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].old_id(), ObjectId::null());
        assert_eq!(entries[0].new_id(), first);
        assert_eq!(entries[1].old_id(), first);
        assert_eq!(entries[1].new_id(), second);
        assert_eq!(entries[1].message(), b"commit: second");
        assert!(entries[0].committer().has_unknown_timezone());
        assert!(
            !fs.exists(Path::new("repo/.git/logs/refs/heads/main.lock"))
                .unwrap()
        );
    }

    #[test]
    fn deletes_expires_rewrites_and_updates_reflogs_transactionally() {
        let (repository, fs) = repository();
        let name = ReferenceName::branch("main").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        let third = ObjectId::compute(crate::ObjectKind::Blob, b"third");
        for (index, (id, previous)) in [
            (first, PreviousValue::MustNotExist),
            (second, PreviousValue::MustExist(first)),
            (third, PreviousValue::MustExist(second)),
        ]
        .into_iter()
        .enumerate()
        {
            repository
                .update_reference_with_reflog(
                    &name,
                    id,
                    previous,
                    &Signature::new(
                        "Test",
                        "test@example.com",
                        i64::try_from(index).unwrap() + 1,
                        0,
                    )
                    .unwrap(),
                    format!("entry {index}").as_bytes(),
                )
                .unwrap();
        }

        let before = fs
            .read(Path::new("repo/.git/logs/refs/heads/main"))
            .unwrap();
        let dry_run = repository
            .delete_reflog_entries(
                name.as_str(),
                &[1],
                &ReflogRewriteOptions {
                    dry_run: true,
                    rewrite: true,
                    update_reference: true,
                    ..ReflogRewriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(dry_run.removed, 1);
        assert_eq!(
            fs.read(Path::new("repo/.git/logs/refs/heads/main"))
                .unwrap(),
            before
        );

        repository
            .delete_reflog_entries(
                name.as_str(),
                &[1],
                &ReflogRewriteOptions {
                    rewrite: true,
                    ..ReflogRewriteOptions::default()
                },
            )
            .unwrap();
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].old_id(), first);
        assert_eq!(entries[1].new_id(), third);

        let outcome = repository
            .expire_reflog_before(
                name.as_str(),
                3,
                &ReflogRewriteOptions {
                    rewrite: true,
                    update_reference: true,
                    ..ReflogRewriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.new_tip, Some(third));
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].old_id(), ObjectId::null());
        assert_eq!(repository.resolve_reference(name.as_str()).unwrap(), third);
        assert!(repository.read_reflog_bounded(name.as_str(), 0).is_err());
        assert!(
            repository
                .delete_reflog_entries(name.as_str(), &[9], &ReflogRewriteOptions::default())
                .is_err()
        );
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/main.lock"))
                .unwrap()
        );
        assert!(
            !fs.exists(Path::new("repo/.git/logs/refs/heads/main.lock"))
                .unwrap()
        );
    }

    #[test]
    fn expires_unreachable_entries_lists_and_drops_logs() {
        let (repository, fs) = repository();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("Test", "test@example.com", 1, 0).unwrap();
        let root = repository
            .write_commit(&CommitBuilder::new(tree, identity.clone(), identity.clone()).build())
            .unwrap();
        let side = repository
            .write_commit(
                &CommitBuilder::new(tree, identity.clone(), identity.clone())
                    .message(b"side".to_vec())
                    .build(),
            )
            .unwrap();
        let tip = repository
            .write_commit(
                &CommitBuilder::new(tree, identity.clone(), identity)
                    .parent(root)
                    .message(b"tip".to_vec())
                    .build(),
            )
            .unwrap();
        let name = ReferenceName::branch("main").unwrap();
        for (index, (id, previous)) in [
            (root, PreviousValue::MustNotExist),
            (side, PreviousValue::MustExist(root)),
            (tip, PreviousValue::MustExist(side)),
        ]
        .into_iter()
        .enumerate()
        {
            repository
                .update_reference_with_reflog(
                    &name,
                    id,
                    previous,
                    &Signature::new(
                        "Test",
                        "test@example.com",
                        i64::try_from(index).unwrap() + 1,
                        0,
                    )
                    .unwrap(),
                    b"move",
                )
                .unwrap();
        }

        let outcome = repository
            .expire_reflog_unreachable_before(
                name.as_str(),
                3,
                &GraphOptions::default(),
                &ReflogRewriteOptions {
                    rewrite: true,
                    ..ReflogRewriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(outcome.removed, 1);
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].new_id(), root);
        assert_eq!(entries[1].old_id(), root);
        assert_eq!(entries[1].new_id(), tip);
        assert_eq!(repository.reflogs(10, 10).unwrap(), vec!["refs/heads/main"]);
        assert!(repository.drop_reflog(name.as_str()).unwrap());
        assert!(!repository.drop_reflog(name.as_str()).unwrap());
        assert!(repository.reflogs(10, 10).unwrap().is_empty());
        assert!(
            !fs.exists(Path::new("repo/.git/logs/refs/heads/main.lock"))
                .unwrap()
        );
    }

    #[test]
    fn stale_fix_prunes_entries_with_broken_commit_closures() {
        let (repository, _) = repository();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("Test", "test@example.com", 1, 0).unwrap();
        let valid = repository
            .write_commit(&CommitBuilder::new(tree, identity.clone(), identity.clone()).build())
            .unwrap();
        let missing = ObjectId::from_str(FIRST).unwrap();
        let name = ReferenceName::branch("broken").unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                valid,
                PreviousValue::MustNotExist,
                &identity,
                b"valid",
            )
            .unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                missing,
                PreviousValue::MustExist(valid),
                &identity,
                b"broken",
            )
            .unwrap();

        let outcome = repository
            .prune_stale_reflog_entries(
                name.as_str(),
                &GraphOptions::default(),
                &ReflogRewriteOptions {
                    rewrite: true,
                    ..ReflogRewriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(outcome.removed, 1);
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].new_id(), valid);
        assert_eq!(entries[0].old_id(), ObjectId::null());
    }

    #[test]
    fn expire_reflog_removes_old_entries() {
        let (repository, _) = repository();
        let name = ReferenceName::branch("main").unwrap();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();

        repository
            .update_reference_with_reflog(
                &name,
                first,
                PreviousValue::MustNotExist,
                &Signature::new("A", "a@example.com", 100, 0).unwrap(),
                b"first",
            )
            .unwrap();
        repository
            .update_reference_with_reflog(
                &name,
                second,
                PreviousValue::Any,
                &Signature::new("B", "b@example.com", 200, 0).unwrap(),
                b"second",
            )
            .unwrap();

        let outcome = repository
            .expire_reflog_before(
                name.as_str(),
                150,
                &ReflogRewriteOptions::default(),
            )
            .unwrap();
        assert_eq!(outcome.removed, 1, "expected 1 entry expired (timestamp=100 < 150)");
        let entries = repository.read_reflog(name.as_str()).unwrap();
        assert_eq!(entries.len(), 1, "expected 1 entry remaining");
        assert_eq!(entries[0].new_id(), second, "remaining entry should be the newer one");
    }

    #[test]
    fn mixed_transaction_updates_direct_and_symbolic_refs_atomically() {
        let (repository, fs) = repository();
        let first = ObjectId::from_str(FIRST).unwrap();
        let second = ObjectId::from_str(SECOND).unwrap();
        let main = ReferenceName::branch("main").unwrap();
        let topic = ReferenceName::branch("topic").unwrap();
        repository
            .update_reference(&main, first, PreviousValue::MustNotExist)
            .unwrap();
        repository
            .update_symbolic_reference("HEAD", &main, PreviousReferenceValue::Any, None)
            .unwrap();
        repository
            .apply_mixed_reference_transaction(&[
                ReferenceTransactionEdit::update(
                    main.as_str(),
                    ReferenceTarget::Direct(second),
                    PreviousReferenceValue::MustExist(ReferenceTarget::Direct(first)),
                ),
                ReferenceTransactionEdit::update(
                    "HEAD",
                    ReferenceTarget::Symbolic(topic.clone()),
                    PreviousReferenceValue::MustExist(ReferenceTarget::Symbolic(main.clone())),
                ),
                ReferenceTransactionEdit::update(
                    topic.as_str(),
                    ReferenceTarget::Direct(first),
                    PreviousReferenceValue::MustNotExist,
                ),
            ])
            .unwrap();
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), second);
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), first);

        assert!(
            repository
                .apply_mixed_reference_transaction(&[
                    ReferenceTransactionEdit::update(
                        main.as_str(),
                        ReferenceTarget::Direct(first),
                        PreviousReferenceValue::MustExist(ReferenceTarget::Direct(first)),
                    ),
                    ReferenceTransactionEdit::update(
                        "HEAD",
                        ReferenceTarget::Symbolic(main.clone()),
                        PreviousReferenceValue::MustExist(ReferenceTarget::Symbolic(topic)),
                    ),
                ])
                .is_err()
        );
        assert_eq!(repository.resolve_reference(main.as_str()).unwrap(), second);
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), first);
        assert!(!fs.exists(Path::new("repo/.git/HEAD.lock")).unwrap());
        assert!(
            !fs.exists(Path::new("repo/.git/refs/heads/main.lock"))
                .unwrap()
        );
    }
}
