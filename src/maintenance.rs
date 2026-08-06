//! Ordered, repository-wide maintenance coordination.

use crate::{
    CommitGraphOptions, CommitGraphReport, Error, GcOptions, GcReport, GraphOptions,
    MultiPackIndexOptions, MultiPackIndexReport, PackRefsOptions, PackRefsResult,
    ReflogRewriteOptions, Repository, Result, WorktreePruneEntry, WorktreePruneOptions,
};

const LOCK_PATH: &str = "objects/maintenance.lock";

/// A native maintenance task supported by the coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceTask {
    Gc,
    CommitGraph,
    MultiPackIndex,
    PackRefs,
    ReflogExpire,
    WorktreePrune,
}

/// Task-specific results, retained in requested execution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceOutcome {
    Gc(Box<GcReport>),
    CommitGraph(CommitGraphReport),
    MultiPackIndex(MultiPackIndexReport),
    PackRefs(PackRefsResult),
    ReflogExpire {
        reflogs: usize,
        removed: usize,
    },
    WorktreePrune(Vec<WorktreePruneEntry>),
    /// A task had no applicable repository data, such as MIDX on no packs.
    Skipped(MaintenanceTask),
}

/// Completed task outcomes and run policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceReport {
    pub dry_run: bool,
    outcomes: Vec<MaintenanceOutcome>,
}

impl MaintenanceReport {
    #[must_use]
    pub fn outcomes(&self) -> &[MaintenanceOutcome] {
        &self.outcomes
    }
}

/// Ordered tasks and their bounded native options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceOptions {
    pub tasks: Vec<MaintenanceTask>,
    pub dry_run: bool,
    /// Explicit authority required for a mutating run.
    pub force: bool,
    pub gc: GcOptions,
    pub commit_graph: CommitGraphOptions,
    pub multi_pack_index: MultiPackIndexOptions,
    pub pack_refs: PackRefsOptions,
    pub reflog_expire_before: Option<i64>,
    pub reflog_expire_unreachable_before: Option<i64>,
    pub reflog: ReflogRewriteOptions,
    pub graph: GraphOptions,
    pub max_reflogs: usize,
    pub max_reflog_depth: usize,
    pub worktree_prune: WorktreePruneOptions,
}

impl Default for MaintenanceOptions {
    fn default() -> Self {
        Self {
            tasks: vec![MaintenanceTask::Gc],
            dry_run: true,
            force: false,
            gc: GcOptions::default(),
            commit_graph: CommitGraphOptions::default(),
            multi_pack_index: MultiPackIndexOptions::default(),
            pack_refs: PackRefsOptions::default(),
            reflog_expire_before: None,
            reflog_expire_unreachable_before: None,
            reflog: ReflogRewriteOptions::default(),
            graph: GraphOptions::default(),
            max_reflogs: 10_000_000,
            max_reflog_depth: 4096,
            worktree_prune: WorktreePruneOptions::default(),
        }
    }
}

impl MaintenanceOptions {
    /// A complete optimization sequence: GC first, then rebuild acceleration
    /// metadata from the resulting object and reference layout.
    #[must_use]
    pub fn full() -> Self {
        Self {
            tasks: vec![
                MaintenanceTask::Gc,
                MaintenanceTask::CommitGraph,
                MaintenanceTask::MultiPackIndex,
            ],
            ..Self::default()
        }
    }
}

impl Repository {
    /// Execute maintenance tasks in caller-specified order under one ODB lock.
    ///
    /// Dry runs perform bounded discovery and encoding but do not acquire the
    /// maintenance lock or publish/remove data. Mutating runs require `force`.
    /// Completed earlier tasks are not rolled back if a later task fails.
    ///
    /// # Errors
    /// Returns an error for missing mutation authority, an empty task list,
    /// lock contention, invalid task policy, corruption, resource-limit
    /// violations, or storage failures.
    pub fn run_maintenance(&self, options: &MaintenanceOptions) -> Result<MaintenanceReport> {
        validate_options(options)?;
        if options.dry_run {
            return self.run_maintenance_tasks(options);
        }
        let lock = self.git_path(LOCK_PATH);
        self.filesystem()
            .write_new(&lock, b"git-rs maintenance\n")?;
        let result = self.run_maintenance_tasks(options);
        let cleanup = self.filesystem().remove_file(&lock);
        match (result, cleanup) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(report), Ok(())) => Ok(report),
        }
    }

    fn run_maintenance_tasks(&self, options: &MaintenanceOptions) -> Result<MaintenanceReport> {
        let mut outcomes = Vec::with_capacity(options.tasks.len());
        for task in &options.tasks {
            outcomes.push(match task {
                MaintenanceTask::Gc => {
                    MaintenanceOutcome::Gc(Box::new(self.maintenance_gc(options)?))
                }
                MaintenanceTask::CommitGraph => {
                    let mut task_options = options.commit_graph.clone();
                    task_options.dry_run = options.dry_run;
                    MaintenanceOutcome::CommitGraph(
                        self.write_commit_graph_reachable(&task_options)?,
                    )
                }
                MaintenanceTask::MultiPackIndex => {
                    if self.has_pack_indexes()? {
                        let mut task_options = options.multi_pack_index.clone();
                        task_options.dry_run = options.dry_run;
                        MaintenanceOutcome::MultiPackIndex(
                            self.write_multi_pack_index(&task_options)?,
                        )
                    } else {
                        MaintenanceOutcome::Skipped(*task)
                    }
                }
                MaintenanceTask::PackRefs => {
                    let mut task_options = options.pack_refs.clone();
                    task_options.all = true;
                    task_options.prune = true;
                    task_options.dry_run = options.dry_run;
                    MaintenanceOutcome::PackRefs(self.pack_refs(&task_options)?)
                }
                MaintenanceTask::ReflogExpire => self.maintenance_reflogs(options)?,
                MaintenanceTask::WorktreePrune => {
                    let mut task_options = options.worktree_prune.clone();
                    task_options.dry_run = options.dry_run;
                    MaintenanceOutcome::WorktreePrune(self.prune_worktrees(&task_options)?)
                }
            });
        }
        Ok(MaintenanceReport {
            dry_run: options.dry_run,
            outcomes,
        })
    }

    fn maintenance_gc(&self, options: &MaintenanceOptions) -> Result<GcReport> {
        let mut task_options = options.gc.clone();
        task_options.dry_run = options.dry_run;
        task_options.force = !options.dry_run;
        self.gc(&task_options)
    }

    fn maintenance_reflogs(&self, options: &MaintenanceOptions) -> Result<MaintenanceOutcome> {
        let logs = self.reflogs(options.max_reflogs, options.max_reflog_depth)?;
        let mut rewrite = options.reflog;
        rewrite.dry_run = options.dry_run;
        let mut removed = 0usize;
        for name in &logs {
            removed = removed
                .checked_add(
                    self.expire_reflog_with_policy(
                        name,
                        options.reflog_expire_before,
                        options.reflog_expire_unreachable_before,
                        &options.graph,
                        &rewrite,
                    )?
                    .removed,
                )
                .ok_or_else(|| Error::InvalidRepository("reflog count overflow".into()))?;
        }
        Ok(MaintenanceOutcome::ReflogExpire {
            reflogs: logs.len(),
            removed,
        })
    }

    fn has_pack_indexes(&self) -> Result<bool> {
        let directory = self.git_path("objects/pack");
        match self.filesystem().read_dir(&directory) {
            Ok(children) => Ok(children
                .iter()
                .any(|path| path.extension().and_then(|value| value.to_str()) == Some("idx"))),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn validate_options(options: &MaintenanceOptions) -> Result<()> {
    if options.tasks.is_empty() {
        return Err(Error::InvalidRepository(
            "maintenance requires at least one task".into(),
        ));
    }
    if !options.dry_run && !options.force {
        return Err(Error::InvalidRepository(
            "maintenance mutation requires force=true".into(),
        ));
    }
    if options.tasks.contains(&MaintenanceTask::ReflogExpire)
        && options.reflog_expire_before.is_none()
        && options.reflog_expire_unreachable_before.is_none()
    {
        return Err(Error::InvalidRepository(
            "reflog-expire maintenance requires an expiry policy".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        CommitOptions, FileSystem, InitOptions, MaintenanceOptions, MaintenanceOutcome,
        MaintenanceTask, MemoryFileSystem, Repository, Signature,
    };

    fn repository() -> (Repository, MemoryFileSystem) {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem.write(Path::new("repo/file"), b"data").unwrap();
        repository.add("file").unwrap();
        let signature = Signature::new("M", "m@example.com", 100, 0).unwrap();
        repository
            .commit_index(b"one", &signature, &signature, &CommitOptions::default())
            .unwrap();
        (repository, filesystem)
    }

    #[test]
    fn full_maintenance_orders_gc_before_acceleration_metadata() {
        let (repository, filesystem) = repository();
        let report = repository
            .run_maintenance(&MaintenanceOptions {
                dry_run: false,
                force: true,
                ..MaintenanceOptions::full()
            })
            .unwrap();
        assert!(matches!(
            report.outcomes(),
            [
                MaintenanceOutcome::Gc(_),
                MaintenanceOutcome::CommitGraph(_),
                MaintenanceOutcome::MultiPackIndex(_)
            ]
        ));
        assert!(
            filesystem
                .exists(Path::new("repo/.git/objects/info/commit-graph"))
                .unwrap()
        );
        assert!(
            filesystem
                .exists(Path::new("repo/.git/objects/pack/multi-pack-index"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/objects/maintenance.lock"))
                .unwrap()
        );
    }

    #[test]
    fn dry_run_preserves_storage_and_reports_skipped_midx() {
        let (repository, filesystem) = repository();
        let report = repository
            .run_maintenance(&MaintenanceOptions {
                tasks: vec![MaintenanceTask::MultiPackIndex],
                ..MaintenanceOptions::default()
            })
            .unwrap();
        assert_eq!(
            report.outcomes(),
            &[MaintenanceOutcome::Skipped(MaintenanceTask::MultiPackIndex)]
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/objects/maintenance.lock"))
                .unwrap()
        );
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/objects/pack/multi-pack-index"))
                .unwrap()
        );
    }

    #[test]
    fn mutation_requires_authority_and_respects_maintenance_lock() {
        let (repository, filesystem) = repository();
        assert!(
            repository
                .run_maintenance(&MaintenanceOptions {
                    dry_run: false,
                    ..MaintenanceOptions::default()
                })
                .is_err()
        );
        filesystem
            .write(Path::new("repo/.git/objects/maintenance.lock"), b"other\n")
            .unwrap();
        assert!(
            repository
                .run_maintenance(&MaintenanceOptions {
                    dry_run: false,
                    force: true,
                    ..MaintenanceOptions::default()
                })
                .is_err()
        );
        assert_eq!(
            filesystem
                .read(Path::new("repo/.git/objects/maintenance.lock"))
                .unwrap(),
            b"other\n"
        );
    }
}
