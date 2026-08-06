//! Repository-wide garbage collection over bounded native maintenance stages.

use crate::{
    Error, GraphOptions, PackRefsOptions, PruneEntry, PruneOptions, ReflogRewriteOptions,
    RepackOptions, RepackResult, Repository, Result, WorktreePruneEntry, WorktreePruneOptions,
};

/// Stages, expiration cutoffs, and resource settings for garbage collection.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcOptions {
    pub dry_run: bool,
    /// Explicit authority required for any mutating run.
    pub force: bool,
    pub pack_refs: bool,
    pub repack: bool,
    /// Total reflog expiry; entries strictly older than this are removed.
    pub reflog_expire_before: Option<i64>,
    /// Unreachable reflog expiry; old unreachable endpoints are removed.
    pub reflog_expire_unreachable_before: Option<i64>,
    /// Prune missing linked-worktree metadata at or before this timestamp.
    pub worktree_expire_before: Option<u64>,
    /// Prune loose unreachable objects at or before this timestamp.
    pub prune_expire_before: Option<u64>,
    pub pack_refs_options: PackRefsOptions,
    pub reflog_options: ReflogRewriteOptions,
    pub graph: GraphOptions,
    pub worktree_options: WorktreePruneOptions,
    pub repack_options: RepackOptions,
    pub prune_options: PruneOptions,
    pub max_reflogs: usize,
    pub max_reflog_depth: usize,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            force: false,
            pack_refs: true,
            repack: true,
            reflog_expire_before: None,
            reflog_expire_unreachable_before: None,
            worktree_expire_before: None,
            prune_expire_before: None,
            pack_refs_options: PackRefsOptions::default(),
            reflog_options: ReflogRewriteOptions::default(),
            graph: GraphOptions::default(),
            worktree_options: WorktreePruneOptions::default(),
            repack_options: RepackOptions::default(),
            prune_options: PruneOptions::default(),
            max_reflogs: 10_000_000,
            max_reflog_depth: 4096,
        }
    }
}

/// Auditable results from each enabled GC stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcReport {
    pub refs_selected: usize,
    pub refs_pruned: usize,
    pub reflogs_scanned: usize,
    pub reflog_entries_removed: usize,
    pub worktrees: Vec<WorktreePruneEntry>,
    pub repack: Option<RepackResult>,
    pub loose_objects: Vec<PruneEntry>,
}

impl Repository {
    /// Run native repository garbage collection in Git's maintenance order.
    ///
    /// The sequence is pack refs, expire reflogs, prune worktree metadata,
    /// cruft-aware repack, then loose-object pruning. Mutating runs hold the
    /// repository's `gc.pid` lock for the entire sequence.
    ///
    /// # Errors
    /// Returns an error for missing mutation authority or explicit prune
    /// expiry, maintenance contention, precious-object policy, a failed stage,
    /// corruption, exceeded bounds, or storage failure.
    pub fn gc(&self, options: &GcOptions) -> Result<GcReport> {
        validate_gc_options(options)?;
        if options.dry_run {
            return self.gc_stages(options);
        }
        let lock = self.git_path("gc.pid");
        self.filesystem().write_new(&lock, b"git-rs gc\n")?;
        let result = self.gc_stages(options);
        let cleanup = self.filesystem().remove_file(&lock);
        match (result, cleanup) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(report), Ok(())) => Ok(report),
        }
    }

    fn gc_stages(&self, options: &GcOptions) -> Result<GcReport> {
        let mut report = GcReport {
            refs_selected: 0,
            refs_pruned: 0,
            reflogs_scanned: 0,
            reflog_entries_removed: 0,
            worktrees: Vec::new(),
            repack: None,
            loose_objects: Vec::new(),
        };
        if options.pack_refs {
            let mut pack_options = options.pack_refs_options.clone();
            pack_options.all = true;
            pack_options.prune = true;
            pack_options.dry_run = options.dry_run;
            let packed = self.pack_refs(&pack_options)?;
            report.refs_selected = packed.packed().len();
            report.refs_pruned = packed.pruned;
        }
        self.gc_expire_reflogs(options, &mut report)?;
        if let Some(expiry) = options.worktree_expire_before {
            let mut worktree = options.worktree_options.clone();
            worktree.expire_before = expiry;
            worktree.dry_run = options.dry_run;
            report.worktrees = self.prune_worktrees(&worktree)?;
        }
        if options.repack {
            let mut repack = options.repack_options.clone();
            repack.include_unreachable = false;
            repack.cruft = true;
            repack.prune_loose = true;
            repack.delete_redundant_packs = true;
            repack.dry_run = options.dry_run;
            report.repack = Some(self.repack(&repack)?);
        }
        if let Some(expiry) = options.prune_expire_before {
            let mut prune = options.prune_options.clone();
            prune.expire_before = Some(expiry);
            prune.force = !options.dry_run;
            prune.dry_run = options.dry_run;
            report.loose_objects = self.prune_under_maintenance_lock(&prune)?;
        }
        Ok(report)
    }

    fn gc_expire_reflogs(&self, options: &GcOptions, report: &mut GcReport) -> Result<()> {
        if options.reflog_expire_before.is_none()
            && options.reflog_expire_unreachable_before.is_none()
        {
            return Ok(());
        }
        let logs = self.reflogs(options.max_reflogs, options.max_reflog_depth)?;
        report.reflogs_scanned = logs.len();
        let mut rewrite = options.reflog_options;
        rewrite.dry_run = options.dry_run;
        for name in logs {
            report.reflog_entries_removed += self
                .expire_reflog_with_policy(
                    &name,
                    options.reflog_expire_before,
                    options.reflog_expire_unreachable_before,
                    &options.graph,
                    &rewrite,
                )?
                .removed;
        }
        Ok(())
    }
}

fn validate_gc_options(options: &GcOptions) -> Result<()> {
    if !options.dry_run && !options.force {
        return Err(Error::InvalidRepository(
            "gc mutation requires force=true".into(),
        ));
    }
    if options.repack_options.cruft_expire_before.is_some() && !options.repack {
        return Err(Error::InvalidRepository(
            "cruft expiration requires the repack stage".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::GcOptions;
    use crate::{
        CommitOptions, FileSystem, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature,
    };

    fn fixture() -> (
        Repository,
        MemoryFileSystem,
        crate::ObjectId,
        crate::ObjectId,
    ) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(Path::new("repo/file"), b"content")
            .unwrap();
        repository.add("file").unwrap();
        let signature = Signature::new("GC", "gc@example.com", 100, 0).unwrap();
        let commit = repository
            .commit_index(b"base", &signature, &signature, &CommitOptions::default())
            .unwrap();
        let orphan = repository
            .write_object(ObjectKind::Blob, b"orphan")
            .unwrap();
        filesystem
            .create_dir_all(Path::new("repo/.git/worktrees/stale"))
            .unwrap();
        filesystem
            .write(
                Path::new("repo/.git/worktrees/stale/gitdir"),
                b"../../../../missing/.git\n",
            )
            .unwrap();
        (repository, filesystem, commit, orphan)
    }

    fn options(dry_run: bool) -> GcOptions {
        GcOptions {
            dry_run,
            force: !dry_run,
            reflog_expire_before: Some(1_000),
            worktree_expire_before: Some(0),
            prune_expire_before: Some(0),
            ..GcOptions::default()
        }
    }

    #[test]
    fn dry_run_previews_every_stage_without_mutation() {
        let (repository, filesystem, _, orphan) = fixture();
        let before = filesystem.read_dir(Path::new("repo/.git/objects")).unwrap();
        let report = repository.gc(&options(true)).unwrap();
        assert_eq!(report.refs_selected, 1);
        assert_eq!(report.refs_pruned, 0);
        assert!(report.reflog_entries_removed > 0);
        assert_eq!(report.worktrees.len(), 1);
        assert!(
            report
                .repack
                .as_ref()
                .is_some_and(|result| result.packed_objects == 4)
        );
        assert!(
            report
                .loose_objects
                .iter()
                .any(|entry| entry.id() == Some(orphan))
        );
        assert!(
            filesystem
                .exists(Path::new("repo/.git/refs/heads/main"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/.git/worktrees/stale"))
                .unwrap()
        );
        assert!(repository.contains_loose_object(orphan).unwrap());
        assert_eq!(
            filesystem.read_dir(Path::new("repo/.git/objects")).unwrap(),
            before
        );
    }

    #[test]
    fn mutation_holds_one_lock_and_publishes_lossless_cruft_gc() {
        let (repository, filesystem, commit, orphan) = fixture();
        let report = repository.gc(&options(false)).unwrap();
        assert_eq!(report.refs_pruned, 1);
        assert!(report.reflog_entries_removed > 0);
        assert_eq!(report.worktrees.len(), 1);
        assert!(report.repack.as_ref().unwrap().cruft_pack.is_some());
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/refs/heads/main"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/worktrees/stale"))
                .unwrap()
        );
        assert!(!filesystem.exists(Path::new("repo/.git/gc.pid")).unwrap());
        assert!(repository.read_object(commit, 1024).is_ok());
        assert!(repository.read_object(orphan, 1024).is_ok());
        assert!(!repository.contains_loose_object(orphan).unwrap());

        filesystem
            .write(Path::new("repo/.git/gc.pid"), b"busy\n")
            .unwrap();
        assert!(repository.gc(&options(false)).is_err());
        assert_eq!(
            filesystem.read(Path::new("repo/.git/gc.pid")).unwrap(),
            b"busy\n"
        );
    }
}
