//! Transport-neutral fetch-and-integrate pull orchestration.

use std::path::Path;

use crate::{
    CheckoutOptions, Error, FastForwardMode, FetchOptions, FetchResult, GraphOptions, MergeOptions,
    MergeResult, ObjectId, PreviousValue, RebaseOptions, RebaseResult, ReferenceName,
    ReferenceTarget, Repository, Result, Signature, StatusOptions, UploadPackTransport,
};

/// How a fetched upstream is reconciled with the current branch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PullMode {
    /// Refuse divergent histories; this is the safe default.
    #[default]
    FastForwardOnly,
    Merge,
    Rebase,
}

/// Remote/upstream selection and integration policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullOptions {
    /// Override `branch.<current>.remote`.
    pub remote_name: Option<String>,
    /// Override `branch.<current>.merge`; short names mean `refs/heads/<name>`.
    pub remote_branch: Option<String>,
    pub mode: PullMode,
    pub fetch: FetchOptions,
    pub merge: MergeOptions,
    pub rebase: RebaseOptions,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            remote_name: None,
            remote_branch: None,
            mode: PullMode::FastForwardOnly,
            fetch: FetchOptions::default(),
            merge: MergeOptions::default(),
            rebase: RebaseOptions::default(),
        }
    }
}

/// Integration outcome after a successful fetch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PullIntegration {
    /// An unborn current branch was initialized at the fetched commit.
    Initialized {
        new: ObjectId,
    },
    Merge(MergeResult),
    Rebase(RebaseResult),
}

/// Fetch details, selected upstream, and integration outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullResult {
    pub remote_name: String,
    pub remote_source: ReferenceName,
    pub tracking_reference: ReferenceName,
    pub upstream: ObjectId,
    pub fetch: FetchResult,
    pub integration: PullIntegration,
}

impl Repository {
    /// Fetch a configured remote and integrate the current branch's upstream.
    ///
    /// The caller supplies transport and committer identity. Remote and merge
    /// branch default to `branch.<current>.remote` and `.merge`; explicit options
    /// override configuration. Pull never chooses a divergence policy implicitly.
    ///
    /// # Errors
    /// Returns an error for detached HEAD, missing/ambiguous upstream config or
    /// refspec mapping, absent advertised upstream, fetch/protocol failures,
    /// dirty integration state, graph conflicts, or ref/storage failures.
    pub fn pull<T: UploadPackTransport>(
        &self,
        transport: &mut T,
        options: &PullOptions,
        committer: &Signature,
    ) -> Result<PullResult> {
        let (branch_name, branch_ref) = current_branch(self)?;
        let (remote_name, remote_source, tracking_reference) =
            self.pull_upstream(&branch_name, options)?;
        let original = match self.resolve_reference("HEAD") {
            Ok(id) => Some(id),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let old_upstream = match self.resolve_reference(tracking_reference.as_str()) {
            Ok(id) => Some(id),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let mut fetch_options = options.fetch.clone();
        remote_name.clone_into(&mut fetch_options.remote_name);
        let fetch = self.fetch_remote(&remote_name, transport, &fetch_options)?;
        let advertised = fetch
            .advertisement
            .refs()
            .iter()
            .find(|reference| reference.name() == remote_source.as_str())
            .ok_or_else(|| {
                Error::InvalidRepository(format!(
                    "remote did not advertise `{}`",
                    remote_source.as_str()
                ))
            })?
            .id();
        let upstream = self.resolve_reference(tracking_reference.as_str())?;
        if upstream != advertised {
            return Err(Error::ReferenceConflict(format!(
                "tracking ref `{}` does not match advertisement",
                tracking_reference.as_str()
            )));
        }
        self.write_fetch_head(upstream, &remote_name, &remote_source)?;

        let integration = if let Some(head) = original {
            match options.mode {
                PullMode::FastForwardOnly => {
                    let mut merge = options.merge.clone();
                    merge.fast_forward = FastForwardMode::Only;
                    PullIntegration::Merge(self.merge(upstream, &merge, committer)?)
                }
                PullMode::Merge => {
                    PullIntegration::Merge(self.merge(upstream, &options.merge, committer)?)
                }
                PullMode::Rebase => {
                    if self.is_ancestor(upstream, head, &options.rebase.graph)? {
                        PullIntegration::Rebase(RebaseResult::UpToDate { head })
                    } else {
                        let boundary = match old_upstream {
                            Some(boundary) => boundary,
                            None => {
                                self.single_pull_merge_base(head, upstream, &options.rebase.graph)?
                            }
                        };
                        PullIntegration::Rebase(self.rebase(
                            boundary,
                            upstream,
                            &options.rebase,
                            committer,
                        )?)
                    }
                }
            }
        } else {
            if !self
                .status(&StatusOptions {
                    include_untracked: false,
                    max_object_size: options.fetch.max_object_size,
                })?
                .is_clean()
            {
                return Err(Error::InvalidRepository(
                    "cannot pull into an unborn branch with staged changes".into(),
                ));
            }
            let commit = self.read_commit(upstream, options.fetch.max_object_size)?;
            self.checkout_tree(
                commit.tree(),
                &CheckoutOptions {
                    force: false,
                    max_object_size: options.fetch.max_object_size,
                },
            )?;
            self.update_reference(&branch_ref, upstream, PreviousValue::MustNotExist)?;
            PullIntegration::Initialized { new: upstream }
        };
        Ok(PullResult {
            remote_name,
            remote_source,
            tracking_reference,
            upstream,
            fetch,
            integration,
        })
    }

    fn pull_upstream(
        &self,
        branch: &str,
        options: &PullOptions,
    ) -> Result<(String, ReferenceName, ReferenceName)> {
        let config = self.read_config()?;
        let remote_name = options.remote_name.clone().map_or_else(
            || config_text(&config, &format!("branch.{branch}.remote"), "branch remote"),
            Ok,
        )?;
        if remote_name == "." {
            return Err(Error::InvalidRepository(
                "local-dot pull remotes do not use an upload-pack transport".into(),
            ));
        }
        let remote = self.remote(&remote_name)?;
        let source = options.remote_branch.clone().map_or_else(
            || {
                config_text(
                    &config,
                    &format!("branch.{branch}.merge"),
                    "branch merge ref",
                )
            },
            Ok,
        )?;
        let source = if source.starts_with("refs/") {
            source
        } else {
            format!("refs/heads/{source}")
        };
        let remote_source = ReferenceName::new(source)?;
        let destinations = remote
            .fetch_refspecs()
            .iter()
            .filter_map(|spec| spec.map_destination(remote_source.as_str()))
            .collect::<std::collections::BTreeSet<_>>();
        if destinations.len() != 1 {
            return Err(Error::InvalidRepository(format!(
                "upstream `{}` has {} fetch destinations",
                remote_source.as_str(),
                destinations.len()
            )));
        }
        let tracking_reference = ReferenceName::new(
            destinations
                .into_iter()
                .next()
                .expect("one destination checked"),
        )?;
        Ok((remote_name, remote_source, tracking_reference))
    }

    fn single_pull_merge_base(
        &self,
        head: ObjectId,
        upstream: ObjectId,
        graph: &GraphOptions,
    ) -> Result<ObjectId> {
        let bases = self.merge_bases(head, upstream, graph)?;
        if bases.len() != 1 {
            return Err(Error::InvalidRepository(
                "pull rebase requires one merge base without prior tracking state".into(),
            ));
        }
        Ok(bases[0])
    }

    fn write_fetch_head(&self, id: ObjectId, remote: &str, source: &ReferenceName) -> Result<()> {
        let label = source
            .as_str()
            .strip_prefix("refs/heads/")
            .unwrap_or(source.as_str());
        self.write_atomic(
            Path::new("FETCH_HEAD"),
            format!("{id}\t\tbranch '{label}' of {remote}\n").as_bytes(),
        )
    }
}

fn current_branch(repository: &Repository) -> Result<(String, ReferenceName)> {
    match repository.read_reference("HEAD")?.target() {
        ReferenceTarget::Symbolic(name) => {
            let branch = name
                .as_str()
                .strip_prefix("refs/heads/")
                .ok_or_else(|| {
                    Error::InvalidRepository("HEAD does not name a local branch".into())
                })?
                .to_owned();
            Ok((branch, name.clone()))
        }
        ReferenceTarget::Direct(_) => Err(Error::InvalidRepository(
            "pull requires a current branch; HEAD is detached".into(),
        )),
    }
}

fn config_text(config: &crate::Config, key: &str, label: &str) -> Result<String> {
    let value = config
        .get(key)?
        .and_then(crate::ConfigEntry::value)
        .ok_or_else(|| Error::InvalidRepository(format!("current branch has no {label}")))?;
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| Error::InvalidRepository(format!("{label} is not UTF-8")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CloneOptions, CommitOptions, FileSystem, InitOptions, MemoryFileSystem,
        RepositoryTransport, UploadPackOptions,
    };

    #[test]
    fn pulls_fast_forward_and_writes_fetch_head() {
        let (source, source_fs, client, client_fs, root, signature) = fixture();
        let remote = commit_files(&source, &source_fs, b"remote\n", b"base\n", &signature);
        let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
        let result = client
            .pull(&mut transport, &PullOptions::default(), &signature)
            .unwrap();
        assert_eq!(result.upstream, remote);
        assert_eq!(
            result.integration,
            PullIntegration::Merge(MergeResult::FastForward {
                old: root,
                new: remote,
            })
        );
        assert_eq!(client.resolve_reference("HEAD").unwrap(), remote);
        assert_eq!(client_fs.read(Path::new("client/a")).unwrap(), b"remote\n");
        let fetch_head = client.read_git_file("FETCH_HEAD").unwrap();
        assert_eq!(
            fetch_head,
            format!("{remote}\t\tbranch 'main' of origin\n").as_bytes()
        );
    }

    #[test]
    fn explicit_merge_policy_creates_merge_commit_for_divergence() {
        let (source, source_fs, client, client_fs, _, signature) = fixture();
        let local = commit_files(&client, &client_fs, b"base\n", b"local\n", &signature);
        let remote = commit_files(&source, &source_fs, b"remote\n", b"base\n", &signature);
        let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
        let result = client
            .pull(
                &mut transport,
                &PullOptions {
                    mode: PullMode::Merge,
                    ..PullOptions::default()
                },
                &signature,
            )
            .unwrap();
        let PullIntegration::Merge(MergeResult::Merged { commit }) = result.integration else {
            panic!("expected merge commit");
        };
        let merged = client.read_commit(commit, 4096).unwrap();
        assert_eq!(merged.parents(), &[local, remote]);
        assert_eq!(client_fs.read(Path::new("client/a")).unwrap(), b"remote\n");
        assert_eq!(client_fs.read(Path::new("client/b")).unwrap(), b"local\n");
    }

    #[test]
    fn rebase_policy_replays_local_commits_on_fetched_tip() {
        let (source, source_fs, client, client_fs, _, signature) = fixture();
        let local = commit_files(&client, &client_fs, b"base\n", b"local\n", &signature);
        let remote = commit_files(&source, &source_fs, b"remote\n", b"base\n", &signature);
        let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
        let result = client
            .pull(
                &mut transport,
                &PullOptions {
                    mode: PullMode::Rebase,
                    ..PullOptions::default()
                },
                &signature,
            )
            .unwrap();
        let PullIntegration::Rebase(RebaseResult::Completed {
            old,
            new,
            replayed: 1,
            dropped: 0,
        }) = result.integration
        else {
            panic!("expected completed rebase");
        };
        assert_eq!(old, local);
        assert_ne!(new, local);
        assert_eq!(client.read_commit(new, 4096).unwrap().parents(), &[remote]);
        assert_eq!(client_fs.read(Path::new("client/a")).unwrap(), b"remote\n");
        assert_eq!(client_fs.read(Path::new("client/b")).unwrap(), b"local\n");
    }

    #[test]
    fn default_policy_rejects_divergence_after_fetching() {
        let (source, source_fs, client, client_fs, _, signature) = fixture();
        let local = commit_files(&client, &client_fs, b"base\n", b"local\n", &signature);
        let remote = commit_files(&source, &source_fs, b"remote\n", b"base\n", &signature);
        let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
        assert!(
            client
                .pull(&mut transport, &PullOptions::default(), &signature)
                .is_err()
        );
        assert_eq!(client.resolve_reference("HEAD").unwrap(), local);
        assert_eq!(
            client
                .resolve_reference("refs/remotes/origin/main")
                .unwrap(),
            remote
        );
    }

    fn fixture() -> (
        Repository,
        MemoryFileSystem,
        Repository,
        MemoryFileSystem,
        ObjectId,
        Signature,
    ) {
        let signature = Signature::new("Pull", "pull@example.com", 100, 0).unwrap();
        let source_fs = MemoryFileSystem::new();
        let source =
            Repository::init(source_fs.clone(), "source", &InitOptions::default()).unwrap();
        source_fs.write(Path::new("source/a"), b"base\n").unwrap();
        source_fs.write(Path::new("source/b"), b"base\n").unwrap();
        source.add(".").unwrap();
        let root = source
            .commit_index(b"root\n", &signature, &signature, &CommitOptions::default())
            .unwrap();
        let client_fs = MemoryFileSystem::new();
        let mut transport = RepositoryTransport::new(&source, UploadPackOptions::default());
        let (client, _) = Repository::clone_from(
            client_fs.clone(),
            "client",
            &mut transport,
            &CloneOptions {
                remote_url: "memory://source".into(),
                max_object_size: 4096,
                max_pack_size: 1024 * 1024,
                max_total_inflated_size: 1024 * 1024,
                ..CloneOptions::default()
            },
        )
        .unwrap();
        (source, source_fs, client, client_fs, root, signature)
    }

    fn commit_files(
        repository: &Repository,
        filesystem: &MemoryFileSystem,
        a: &[u8],
        b: &[u8],
        signature: &Signature,
    ) -> ObjectId {
        let root = repository.work_tree().unwrap();
        filesystem.write(&root.join("a"), a).unwrap();
        filesystem.write(&root.join("b"), b).unwrap();
        repository.add(".").unwrap();
        repository
            .commit_index(b"change\n", signature, signature, &CommitOptions::default())
            .unwrap()
    }
}
