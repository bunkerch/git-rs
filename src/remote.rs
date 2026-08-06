//! Named remote configuration and tracking-reference management.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{
    Config, Error, PreviousValue, RefSpec, ReferenceEdit, ReferenceName, ReferenceTarget,
    Repository, Result,
};

/// One configured remote with byte-preserving URLs and typed refspecs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remote {
    name: String,
    urls: Vec<Vec<u8>>,
    push_urls: Vec<Vec<u8>>,
    fetch: Vec<RefSpec>,
    push: Vec<RefSpec>,
}

impl Remote {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn urls(&self) -> &[Vec<u8>] {
        &self.urls
    }

    #[must_use]
    pub fn push_urls(&self) -> &[Vec<u8>] {
        &self.push_urls
    }

    #[must_use]
    pub fn fetch_refspecs(&self) -> &[RefSpec] {
        &self.fetch
    }

    #[must_use]
    pub fn push_refspecs(&self) -> &[RefSpec] {
        &self.push
    }
}

/// References removed or deliberately retained while deleting a remote.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RemoveRemoteResult {
    pub removed_refs: Vec<ReferenceName>,
    /// Configured fetch destinations outside `refs/remotes/<name>/` are not
    /// deleted automatically because they may be shared local namespaces.
    pub retained_refs: Vec<ReferenceName>,
}

#[derive(Default)]
struct RemoteBuilder {
    urls: Vec<Vec<u8>>,
    push_urls: Vec<Vec<u8>>,
    fetch: Vec<RefSpec>,
    push: Vec<RefSpec>,
}

impl Repository {
    /// List configured remotes in bytewise name order.
    ///
    /// # Errors
    /// Returns an error for malformed config, non-UTF-8 remote names/refspecs,
    /// invalid refspecs, or storage failures.
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        parse_remotes(&self.read_config()?)
    }

    /// Read one exact configured remote.
    ///
    /// # Errors
    /// Returns an error for an invalid/missing remote or malformed config.
    pub fn remote(&self, name: &str) -> Result<Remote> {
        validate_remote_name(name)?;
        self.remotes()?
            .into_iter()
            .find(|remote| remote.name == name)
            .ok_or_else(|| Error::InvalidRepository(format!("remote `{name}` is not configured")))
    }

    /// Add a remote with Git's default branch fetch mapping.
    ///
    /// # Errors
    /// Returns an error for invalid/duplicate names, NUL URLs, malformed
    /// config, or atomic config storage failures.
    pub fn add_remote(&self, name: &str, url: impl AsRef<[u8]>) -> Result<Remote> {
        validate_remote_name(name)?;
        let mut config = self.read_config()?;
        if has_remote_section(&config, name.as_bytes()) {
            return Err(Error::InvalidRepository(format!(
                "remote `{name}` already exists"
            )));
        }
        config.set(&format!("remote.{name}.url"), url)?;
        config.set(
            &format!("remote.{name}.fetch"),
            format!("+refs/heads/*:refs/remotes/{name}/*"),
        )?;
        self.write_config(&config)?;
        self.remote(name)
    }

    /// Replace fetch or push URLs. An empty push URL list restores URL fallback;
    /// the ordinary URL list must remain nonempty.
    ///
    /// # Errors
    /// Returns an error for a missing remote, empty fetch URL set, NUL values,
    /// malformed config, or storage failures.
    pub fn set_remote_urls(&self, name: &str, urls: &[Vec<u8>], push: bool) -> Result<Remote> {
        self.remote(name)?;
        if !push && urls.is_empty() {
            return Err(Error::InvalidRepository(
                "remote must have at least one URL".into(),
            ));
        }
        let mut config = self.read_config()?;
        let variable = if push { "pushurl" } else { "url" };
        config.unset_in_subsection("remote", name.as_bytes(), variable)?;
        for url in urls {
            config.add(&format!("remote.{name}.{variable}"), url)?;
        }
        self.write_config(&config)?;
        self.remote(name)
    }

    /// Append a validated fetch refspec.
    ///
    /// # Errors
    /// Returns an error for a missing remote, invalid refspec, or config write.
    pub fn add_remote_fetch_refspec(&self, name: &str, value: &str) -> Result<Remote> {
        self.remote(name)?;
        let spec = RefSpec::parse_fetch(value)?;
        let mut config = self.read_config()?;
        config.add(&format!("remote.{name}.fetch"), spec.to_string())?;
        self.write_config(&config)?;
        self.remote(name)
    }

    /// Append a validated push refspec.
    ///
    /// # Errors
    /// Returns an error for a missing remote, invalid refspec, or config write.
    pub fn add_remote_push_refspec(&self, name: &str, value: &str) -> Result<Remote> {
        self.remote(name)?;
        let spec = RefSpec::parse_push(value)?;
        let mut config = self.read_config()?;
        config.add(&format!("remote.{name}.push"), spec.to_string())?;
        self.write_config(&config)?;
        self.remote(name)
    }

    /// Remove configuration, branch upstream settings, and tracking refs in
    /// the remote's own namespace.
    ///
    /// # Errors
    /// Returns an error for a missing/malformed remote, reference transaction,
    /// or config/storage failure.
    pub fn remove_remote(&self, name: &str) -> Result<RemoveRemoteResult> {
        let remote = self.remote(name)?;
        let mut result = RemoveRemoteResult::default();
        let prefix = format!("refs/remotes/{name}/");
        let mut direct = Vec::new();
        let mut symbolic = Vec::new();
        for reference in self.references()? {
            let own_namespace = reference.name().starts_with(&prefix);
            let configured = remote
                .fetch
                .iter()
                .any(|spec| !spec.is_negative() && spec.matches_destination(reference.name()));
            if !own_namespace && !configured {
                continue;
            }
            let reference_name = ReferenceName::new(reference.name().to_owned())?;
            if !own_namespace {
                result.retained_refs.push(reference_name);
                continue;
            }
            match reference.target() {
                ReferenceTarget::Direct(id) => {
                    direct.push(ReferenceEdit::delete(reference_name, *id));
                }
                ReferenceTarget::Symbolic(_) => symbolic.push(reference_name),
            }
        }
        if !direct.is_empty() {
            self.apply_reference_transaction(&direct)?;
            result
                .removed_refs
                .extend(direct.iter().map(|edit| edit.name().clone()));
        }
        for name in symbolic {
            remove_symbolic_ref(self, &name)?;
            result.removed_refs.push(name);
        }

        let mut config = self.read_config()?;
        config.remove_subsection("remote", name.as_bytes())?;
        clear_branch_remote_settings(&mut config, name)?;
        if config
            .get("remote.pushdefault")?
            .and_then(crate::ConfigEntry::value)
            == Some(name.as_bytes())
        {
            config.unset("remote.pushdefault")?;
        }
        self.write_config(&config)?;
        result
            .removed_refs
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        result
            .retained_refs
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(result)
    }

    /// Rename remote configuration and its standard tracking namespace.
    ///
    /// # Errors
    /// Returns an error for invalid/missing/duplicate remotes, destination ref
    /// conflicts, malformed config, or reference/config storage failures.
    pub fn rename_remote(&self, old: &str, new: &str) -> Result<Remote> {
        validate_remote_name(old)?;
        validate_remote_name(new)?;
        if old == new {
            return self.remote(old);
        }
        self.remote(old)?;
        if self.remotes()?.iter().any(|remote| remote.name == new) {
            return Err(Error::InvalidRepository(format!(
                "remote `{new}` already exists"
            )));
        }
        rename_tracking_refs(self, old, new)?;

        let mut config = self.read_config()?;
        if config.rename_subsection("remote", old.as_bytes(), new.as_bytes())? == 0 {
            return Err(Error::InvalidRepository(format!(
                "remote `{old}` has no config section"
            )));
        }
        rewrite_remote_config(&mut config, old, new)?;
        self.write_config(&config)?;
        self.remote(new)
    }
}

fn parse_remotes(config: &Config) -> Result<Vec<Remote>> {
    let mut builders = BTreeMap::<String, RemoteBuilder>::new();
    for entry in config.entries() {
        if entry.section() != "remote" {
            continue;
        }
        let Some(subsection) = entry.subsection() else {
            continue;
        };
        let name = std::str::from_utf8(subsection)
            .map_err(|_| Error::InvalidRepository("remote name is not UTF-8".into()))?
            .to_owned();
        validate_remote_name(&name)?;
        let value = entry.value().unwrap_or(b"");
        let builder = builders.entry(name).or_default();
        match entry.name() {
            "url" => builder.urls.push(value.to_vec()),
            "pushurl" => builder.push_urls.push(value.to_vec()),
            "fetch" => builder
                .fetch
                .push(RefSpec::parse_fetch(std::str::from_utf8(value).map_err(
                    |_| Error::InvalidRepository("fetch refspec is not UTF-8".into()),
                )?)?),
            "push" => builder
                .push
                .push(RefSpec::parse_push(std::str::from_utf8(value).map_err(
                    |_| Error::InvalidRepository("push refspec is not UTF-8".into()),
                )?)?),
            _ => {}
        }
    }
    Ok(builders
        .into_iter()
        .map(|(name, builder)| Remote {
            name,
            urls: builder.urls,
            push_urls: builder.push_urls,
            fetch: builder.fetch,
            push: builder.push,
        })
        .collect())
}

fn has_remote_section(config: &Config, name: &[u8]) -> bool {
    config
        .entries()
        .iter()
        .any(|entry| entry.section() == "remote" && entry.subsection() == Some(name))
}

fn clear_branch_remote_settings(config: &mut Config, remote: &str) -> Result<()> {
    let settings = config
        .entries()
        .iter()
        .filter(|entry| entry.section() == "branch")
        .filter_map(|entry| {
            let value = entry.value()?;
            (matches!(entry.name(), "remote" | "pushremote") && value == remote.as_bytes())
                .then(|| {
                    entry
                        .subsection()
                        .map(|subsection| (subsection.to_vec(), entry.name().to_owned()))
                })
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    for (branch, setting) in settings {
        config.unset_in_subsection("branch", &branch, &setting)?;
        if setting == "remote" {
            config.unset_in_subsection("branch", &branch, "merge")?;
        }
    }
    Ok(())
}

fn rewrite_remote_config(config: &mut Config, old: &str, new: &str) -> Result<()> {
    let old_prefix = format!("refs/remotes/{old}/");
    let new_prefix = format!("refs/remotes/{new}/");
    let replacements = config
        .entries()
        .iter()
        .filter(|entry| entry.section() == "remote" && entry.subsection() == Some(new.as_bytes()))
        .filter(|entry| entry.name() == "fetch")
        .filter_map(|entry| entry.value())
        .map(|value| String::from_utf8_lossy(value).replace(&old_prefix, &new_prefix))
        .collect::<Vec<_>>();
    config.unset_in_subsection("remote", new.as_bytes(), "fetch")?;
    for value in replacements {
        RefSpec::parse_fetch(&value)?;
        config.add(&format!("remote.{new}.fetch"), value)?;
    }
    for section in ["branch"] {
        let subsections = config
            .entries()
            .iter()
            .filter(|entry| entry.section() == section)
            .filter(|entry| matches!(entry.name(), "remote" | "pushremote"))
            .filter(|entry| entry.value() == Some(old.as_bytes()))
            .filter_map(|entry| entry.subsection().map(<[u8]>::to_vec))
            .collect::<Vec<_>>();
        for subsection in subsections {
            for variable in ["remote", "pushremote"] {
                let entries = config
                    .entries()
                    .iter()
                    .filter(|entry| {
                        entry.section() == section
                            && entry.subsection() == Some(subsection.as_slice())
                            && entry.name() == variable
                            && entry.value() == Some(old.as_bytes())
                    })
                    .count();
                if entries != 0 {
                    config.unset_in_subsection(section, &subsection, variable)?;
                    config.set_in_subsection(section, &subsection, variable, new)?;
                }
            }
        }
    }
    if config
        .get("remote.pushdefault")?
        .and_then(crate::ConfigEntry::value)
        == Some(old.as_bytes())
    {
        config.set("remote.pushdefault", new)?;
    }
    Ok(())
}

fn rename_tracking_refs(repository: &Repository, old: &str, new: &str) -> Result<()> {
    let old_prefix = format!("refs/remotes/{old}/");
    let new_prefix = format!("refs/remotes/{new}/");
    let mut edits = Vec::new();
    let mut symbolic = Vec::new();
    let mut reflogs = Vec::new();
    for reference in repository.references()? {
        let Some(suffix) = reference.name().strip_prefix(&old_prefix) else {
            continue;
        };
        let old_name = ReferenceName::new(reference.name().to_owned())?;
        let new_name = ReferenceName::new(format!("{new_prefix}{suffix}"))?;
        match repository.read_reference(new_name.as_str()) {
            Ok(_) => return Err(Error::ReferenceConflict(new_name.to_string())),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match repository
            .filesystem()
            .read(&repository.git_path(Path::new("logs").join(old_name.as_str())))
        {
            Ok(contents) => reflogs.push((new_name.clone(), contents)),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match reference.target() {
            ReferenceTarget::Direct(id) => {
                edits.push(ReferenceEdit::update(
                    new_name,
                    *id,
                    PreviousValue::MustNotExist,
                ));
                edits.push(ReferenceEdit::delete(old_name, *id));
            }
            ReferenceTarget::Symbolic(target) => {
                let rewritten = target.as_str().replace(&old_prefix, &new_prefix);
                symbolic.push((old_name, new_name, rewritten));
            }
        }
    }
    if !edits.is_empty() {
        repository.apply_reference_transaction(&edits)?;
    }
    for (old_name, new_name, target) in symbolic {
        let destination = repository.git_path(new_name.as_str());
        if let Some(parent) = destination.parent() {
            repository.filesystem().create_dir_all(parent)?;
        }
        repository.write_atomic(
            Path::new(new_name.as_str()),
            format!("ref: {target}\n").as_bytes(),
        )?;
        remove_symbolic_ref(repository, &old_name)?;
    }
    for (name, contents) in reflogs {
        let relative = Path::new("logs").join(name.as_str());
        if let Some(parent) = repository.git_path(&relative).parent() {
            repository.filesystem().create_dir_all(parent)?;
        }
        repository.write_atomic(&relative, &contents)?;
    }
    Ok(())
}

fn remove_symbolic_ref(repository: &Repository, name: &ReferenceName) -> Result<()> {
    let path = repository.git_path(name.as_str());
    match repository.filesystem().remove_file(&path) {
        Ok(()) => {}
        Err(Error::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    }
    let log = repository.git_path(Path::new("logs").join(name.as_str()));
    match repository.filesystem().remove_file(&log) {
        Ok(()) | Err(Error::NotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains(['\0', '\n', '\r', ' ', '\t'])
        || ReferenceName::new(format!("refs/remotes/{name}/probe")).is_err()
    {
        return Err(Error::InvalidReferenceName(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{FileSystem, InitOptions, MemoryFileSystem, ObjectKind};

    #[test]
    fn adds_lists_and_mutates_urls_and_refspecs() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let remote = repository
            .add_remote("origin", b"memory://primary")
            .unwrap();
        assert_eq!(remote.urls(), &[b"memory://primary".to_vec()]);
        assert_eq!(
            remote.fetch_refspecs()[0].to_string(),
            "+refs/heads/*:refs/remotes/origin/*"
        );
        assert!(repository.add_remote("origin", b"duplicate").is_err());

        let remote = repository
            .set_remote_urls(
                "origin",
                &[b"memory://one".to_vec(), b"memory://two".to_vec()],
                false,
            )
            .unwrap();
        assert_eq!(
            remote.urls(),
            &[b"memory://one".to_vec(), b"memory://two".to_vec()]
        );
        let remote = repository
            .set_remote_urls("origin", &[b"memory://push".to_vec()], true)
            .unwrap();
        assert_eq!(remote.push_urls(), &[b"memory://push".to_vec()]);
        let remote = repository
            .add_remote_fetch_refspec("origin", "^refs/heads/private/*")
            .unwrap();
        assert!(remote.fetch_refspecs()[1].is_negative());
        let remote = repository
            .add_remote_push_refspec("origin", "refs/heads/*:refs/backup/*")
            .unwrap();
        assert_eq!(remote.push_refspecs().len(), 1);
        assert_eq!(repository.remotes().unwrap(), vec![remote]);
    }

    #[test]
    fn renames_tracking_refs_fetch_mappings_and_branch_settings() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        repository.add_remote("origin", b"memory://remote").unwrap();
        let id = repository.write_object(ObjectKind::Blob, b"tip").unwrap();
        for name in ["refs/remotes/origin/main", "refs/remotes/origin/topic"] {
            repository
                .update_reference(
                    &ReferenceName::new(name).unwrap(),
                    id,
                    PreviousValue::MustNotExist,
                )
                .unwrap();
        }
        repository
            .write_atomic(
                Path::new("refs/remotes/origin/HEAD"),
                b"ref: refs/remotes/origin/main\n",
            )
            .unwrap();
        let mut config = repository.read_config().unwrap();
        config.set("branch.main.remote", b"origin").unwrap();
        config.set("branch.main.merge", b"refs/heads/main").unwrap();
        config.set("branch.main.pushremote", b"origin").unwrap();
        config.set("remote.pushdefault", b"origin").unwrap();
        repository.write_config(&config).unwrap();

        let renamed = repository.rename_remote("origin", "upstream").unwrap();
        assert_eq!(renamed.name(), "upstream");
        assert_eq!(
            renamed.fetch_refspecs()[0].to_string(),
            "+refs/heads/*:refs/remotes/upstream/*"
        );
        assert_eq!(
            repository
                .resolve_reference("refs/remotes/upstream/main")
                .unwrap(),
            id
        );
        assert!(
            repository
                .resolve_reference("refs/remotes/origin/main")
                .is_err()
        );
        assert_eq!(
            filesystem
                .read(Path::new("repo/.git/refs/remotes/upstream/HEAD"))
                .unwrap(),
            b"ref: refs/remotes/upstream/main\n"
        );
        let config = repository.read_config().unwrap();
        assert_eq!(
            config.get("branch.main.remote").unwrap().unwrap().value(),
            Some(b"upstream".as_slice())
        );
        assert_eq!(
            config
                .get("branch.main.pushremote")
                .unwrap()
                .unwrap()
                .value(),
            Some(b"upstream".as_slice())
        );
        assert_eq!(
            config.get("remote.pushdefault").unwrap().unwrap().value(),
            Some(b"upstream".as_slice())
        );
    }

    #[test]
    fn removes_own_tracking_namespace_and_clears_upstream_config() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        repository.add_remote("origin", b"memory://remote").unwrap();
        repository
            .add_remote_fetch_refspec("origin", "+refs/changes/*:refs/cache/*")
            .unwrap();
        let id = repository.write_object(ObjectKind::Blob, b"tip").unwrap();
        for name in ["refs/remotes/origin/main", "refs/cache/change"] {
            repository
                .update_reference(
                    &ReferenceName::new(name).unwrap(),
                    id,
                    PreviousValue::MustNotExist,
                )
                .unwrap();
        }
        repository
            .write_atomic(
                Path::new("refs/remotes/origin/HEAD"),
                b"ref: refs/remotes/origin/main\n",
            )
            .unwrap();
        let mut config = repository.read_config().unwrap();
        config.set("branch.main.remote", b"origin").unwrap();
        config.set("branch.main.merge", b"refs/heads/main").unwrap();
        config.set("remote.pushdefault", b"origin").unwrap();
        repository.write_config(&config).unwrap();

        let result = repository.remove_remote("origin").unwrap();
        assert_eq!(
            result
                .removed_refs
                .iter()
                .map(ReferenceName::as_str)
                .collect::<Vec<_>>(),
            ["refs/remotes/origin/HEAD", "refs/remotes/origin/main"]
        );
        assert_eq!(
            result
                .retained_refs
                .iter()
                .map(ReferenceName::as_str)
                .collect::<Vec<_>>(),
            ["refs/cache/change"]
        );
        assert_eq!(
            repository.resolve_reference("refs/cache/change").unwrap(),
            id
        );
        assert!(repository.remote("origin").is_err());
        let config = repository.read_config().unwrap();
        assert!(config.get("branch.main.remote").unwrap().is_none());
        assert!(config.get("branch.main.merge").unwrap().is_none());
        assert!(config.get("remote.pushdefault").unwrap().is_none());
        assert!(
            !filesystem
                .exists(Path::new("repo/.git/refs/remotes/origin/HEAD"))
                .unwrap()
        );
    }
}
