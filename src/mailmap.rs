//! Git-compatible identity canonicalization from `.mailmap` data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, ObjectKind, Repository, Result, RevisionOptions, Signature};

/// Resource limits for parsing and loading mailmap sources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailmapOptions {
    pub max_bytes: usize,
    pub max_entries: usize,
}

impl Default for MailmapOptions {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_entries: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct MappedIdentity {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct EmailMappings {
    default: MappedIdentity,
    by_name: BTreeMap<String, MappedIdentity>,
}

/// Parsed case-insensitive mailmap mappings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mailmap {
    by_email: BTreeMap<String, EmailMappings>,
    entries: usize,
}

impl Mailmap {
    /// Parse one complete mailmap source.
    ///
    /// # Errors
    /// Returns an error for malformed entries, non-UTF-8 data, or resource
    /// limits. Blank and comment lines do not count as entries.
    pub fn parse(data: &[u8], options: &MailmapOptions) -> Result<Self> {
        if data.len() > options.max_bytes {
            return Err(Error::InvalidRepository(format!(
                "mailmap exceeds {} bytes",
                options.max_bytes
            )));
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| Error::InvalidRepository("mailmap is not UTF-8".into()))?;
        let mut mailmap = Self::default();
        for (line_number, line) in text.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            mailmap.entries = mailmap
                .entries
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepository("mailmap entry count overflow".into()))?;
            if mailmap.entries > options.max_entries {
                return Err(Error::InvalidRepository(format!(
                    "mailmap exceeds {} entries",
                    options.max_entries
                )));
            }
            mailmap.add_line(line).map_err(|error| match error {
                Error::InvalidRepository(message) => {
                    Error::InvalidRepository(format!("mailmap line {}: {message}", line_number + 1))
                }
                error => error,
            })?;
        }
        Ok(mailmap)
    }

    /// Canonicalize a parsed commit identity while preserving its timestamp.
    ///
    /// # Errors
    /// Returns an error if a mapping cannot form a valid commit identity.
    pub fn map_signature(&self, signature: &Signature) -> Result<Signature> {
        let (name, email) = self.map_identity(signature.name(), signature.email());
        if name == signature.name() && email == signature.email() {
            return Ok(signature.clone());
        }
        if signature.has_unknown_timezone() {
            Ok(Signature::with_unknown_timezone(
                name,
                email,
                signature.timestamp(),
            )?)
        } else {
            Ok(Signature::new(
                name,
                email,
                signature.timestamp(),
                signature.offset_minutes(),
            )?)
        }
    }

    /// Canonicalize a name and email pair.
    #[must_use]
    pub fn map_identity<'a>(&'a self, name: &'a str, email: &'a str) -> (&'a str, &'a str) {
        let Some(mappings) = self.by_email.get(&email.to_ascii_lowercase()) else {
            return (name, email);
        };
        let selected = mappings
            .by_name
            .get(&name.to_ascii_lowercase())
            .unwrap_or(&mappings.default);
        (
            selected.name.as_deref().unwrap_or(name),
            selected.email.as_deref().unwrap_or(email),
        )
    }

    fn add_line(&mut self, line: &str) -> Result<()> {
        let (new_name, new_email, remainder) = parse_name_email(line, false)?;
        let (old_name, old_email) = if remainder.trim().is_empty() {
            (None, None)
        } else {
            let (name, email, trailing) = parse_name_email(remainder, true)?;
            if !trailing.trim().is_empty() {
                return Err(Error::InvalidRepository(
                    "unexpected data after second identity".into(),
                ));
            }
            (name, Some(email))
        };
        let (lookup_email, replacement_email) = match old_email {
            Some(old) if !old.is_empty() => (old, Some(new_email)),
            Some(_) => {
                return Err(Error::InvalidRepository(
                    "old mailmap email is empty".into(),
                ));
            }
            None => (new_email, None),
        };
        let replacement = MappedIdentity {
            name: new_name.map(str::to_owned),
            email: replacement_email.map(str::to_owned),
        };
        let mappings = self
            .by_email
            .entry(lookup_email.to_ascii_lowercase())
            .or_default();
        if let Some(old_name) = old_name {
            mappings
                .by_name
                .insert(old_name.to_ascii_lowercase(), replacement);
        } else {
            if replacement.name.is_some() {
                mappings.default.name = replacement.name;
            }
            if replacement.email.is_some() {
                mappings.default.email = replacement.email;
            }
        }
        Ok(())
    }

    fn merge(&mut self, source: Self, options: &MailmapOptions) -> Result<()> {
        self.entries = self
            .entries
            .checked_add(source.entries)
            .ok_or_else(|| Error::InvalidRepository("mailmap entry count overflow".into()))?;
        if self.entries > options.max_entries {
            return Err(Error::InvalidRepository(format!(
                "mailmap exceeds {} entries",
                options.max_entries
            )));
        }
        for (email, source) in source.by_email {
            let destination = self.by_email.entry(email).or_default();
            if source.default.name.is_some() {
                destination.default.name = source.default.name;
            }
            if source.default.email.is_some() {
                destination.default.email = source.default.email;
            }
            destination.by_name.extend(source.by_name);
        }
        Ok(())
    }
}

impl Repository {
    /// Load Git's configured mailmap sources through the repository filesystem.
    ///
    /// Sources are applied in Git order: worktree `.mailmap`, configured or
    /// bare-repository blob, then `mailmap.file`.
    ///
    /// # Errors
    /// Returns an error for malformed configuration, missing explicit sources,
    /// invalid mailmap data, object errors, or resource-limit violations.
    pub fn load_mailmap(&self, options: &MailmapOptions) -> Result<Mailmap> {
        let config = self.read_config()?;
        let configured_blob = config
            .get("mailmap.blob")?
            .and_then(|entry| entry.value())
            .map(parse_config_string)
            .transpose()?;
        let configured_file = config
            .get("mailmap.file")?
            .and_then(|entry| entry.value())
            .map(parse_config_string)
            .transpose()?;
        let mut result = Mailmap::default();
        let mut loaded_bytes = 0usize;
        if let Some(worktree) = self.work_tree() {
            let path = worktree.join(".mailmap");
            match self.filesystem().read(&path) {
                Ok(data) => merge_source(&mut result, &mut loaded_bytes, &data, options)?,
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let blob = configured_blob
            .as_deref()
            .or_else(|| self.work_tree().is_none().then_some("HEAD:.mailmap"));
        if let Some(blob) = blob {
            match self.resolve_revision_id(blob, &RevisionOptions::default()) {
                Ok(id) => {
                    let object = self.read_object(id, options.max_bytes)?;
                    if object.kind() != ObjectKind::Blob {
                        return Err(Error::InvalidRepository(format!(
                            "mailmap blob `{blob}` is not a blob"
                        )));
                    }
                    merge_source(&mut result, &mut loaded_bytes, object.data(), options)?;
                }
                Err(Error::NotFound(_) | Error::InvalidRevision(_))
                    if configured_blob.is_none() => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(file) = configured_file {
            let path = configured_path(self, &file);
            let data = self.filesystem().read(&path)?;
            merge_source(&mut result, &mut loaded_bytes, &data, options)?;
        }
        Ok(result)
    }
}

fn merge_source(
    destination: &mut Mailmap,
    loaded_bytes: &mut usize,
    data: &[u8],
    options: &MailmapOptions,
) -> Result<()> {
    *loaded_bytes = loaded_bytes
        .checked_add(data.len())
        .ok_or_else(|| Error::InvalidRepository("mailmap byte count overflow".into()))?;
    if *loaded_bytes > options.max_bytes {
        return Err(Error::InvalidRepository(format!(
            "mailmap sources exceed {} bytes",
            options.max_bytes
        )));
    }
    destination.merge(Mailmap::parse(data, options)?, options)
}

fn parse_name_email(input: &str, allow_empty_email: bool) -> Result<(Option<&str>, &str, &str)> {
    let left = input
        .find('<')
        .ok_or_else(|| Error::InvalidRepository("identity has no `<`".into()))?;
    let relative_right = input[left + 1..]
        .find('>')
        .ok_or_else(|| Error::InvalidRepository("identity has no `>`".into()))?;
    let right = left + 1 + relative_right;
    let name = input[..left].trim();
    let email = &input[left + 1..right];
    if email.is_empty() && !allow_empty_email {
        return Err(Error::InvalidRepository("mailmap email is empty".into()));
    }
    validate_component(name, "name")?;
    validate_component(email, "email")?;
    Ok((
        (!name.is_empty()).then_some(name),
        email,
        &input[right + 1..],
    ))
}

fn validate_component(value: &str, kind: &str) -> Result<()> {
    if value.contains(['\0', '\n', '\r', '<', '>']) {
        return Err(Error::InvalidRepository(format!("invalid mailmap {kind}")));
    }
    Ok(())
}

fn parse_config_string(value: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("mailmap config value is non-UTF-8".into()))?
        .trim();
    if value.is_empty() {
        return Err(Error::InvalidRepository(
            "mailmap config value is empty".into(),
        ));
    }
    Ok(value.to_owned())
}

fn configured_path(repository: &Repository, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        repository
            .work_tree()
            .unwrap_or(repository.git_dir())
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{Mailmap, MailmapOptions};
    use crate::{FileSystem, InitOptions, MemoryFileSystem, ObjectKind, Repository};

    #[test]
    fn maps_all_documented_forms_case_insensitively() {
        let map = Mailmap::parse(
            b"Proper One <one@old>\n\
              <proper@two> <two@old>\n\
              Proper Three <proper@three> <three@old>\n\
              Proper Four <proper@four> Commit Four <shared@old>\n",
            &MailmapOptions::default(),
        )
        .unwrap();
        assert_eq!(
            map.map_identity("Alias", "ONE@OLD"),
            ("Proper One", "ONE@OLD")
        );
        assert_eq!(
            map.map_identity("Alias", "two@old"),
            ("Alias", "proper@two")
        );
        assert_eq!(
            map.map_identity("Alias", "three@old"),
            ("Proper Three", "proper@three")
        );
        assert_eq!(
            map.map_identity("commit four", "SHARED@OLD"),
            ("Proper Four", "proper@four")
        );
        assert_eq!(
            map.map_identity("Other", "shared@old"),
            ("Other", "shared@old")
        );
    }

    #[test]
    fn rejects_malformed_and_limited_input() {
        assert!(Mailmap::parse(b"missing email\n", &MailmapOptions::default()).is_err());
        assert!(
            Mailmap::parse(
                b"A <a>\nB <b>\n",
                &MailmapOptions {
                    max_entries: 1,
                    ..MailmapOptions::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn repository_sources_override_in_git_order() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(
                std::path::Path::new("repo/.mailmap"),
                b"Worktree <worktree@example.com> <old@example.com>\n",
            )
            .unwrap();
        let blob = repository
            .write_object(
                ObjectKind::Blob,
                b"Blob <blob@example.com> <old@example.com>\n",
            )
            .unwrap();
        filesystem
            .write(
                std::path::Path::new("repo/final.mailmap"),
                b"Final <final@example.com> <old@example.com>\n",
            )
            .unwrap();
        let mut config = repository.read_config().unwrap();
        config.set("mailmap.blob", blob.to_string()).unwrap();
        config.set("mailmap.file", "final.mailmap").unwrap();
        repository.write_config(&config).unwrap();

        assert_eq!(
            repository
                .load_mailmap(&MailmapOptions::default())
                .unwrap()
                .map_identity("Alias", "old@example.com"),
            ("Final", "final@example.com")
        );
    }
}
