//! Bounded contributor summaries over revision ranges.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Error, LogOptions, MailmapOptions, ObjectId, Repository, Result, RevisionWalkOptions, Signature,
};

/// Identity sources under which commits are grouped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShortlogGroup {
    Author,
    Committer,
    Trailer(String),
}

/// Selection, grouping, mailmap, and aggregate-output bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortlogOptions {
    pub walk: RevisionWalkOptions,
    pub paths: Vec<Vec<u8>>,
    pub groups: Vec<ShortlogGroup>,
    pub include_email: bool,
    pub sort_by_number: bool,
    pub use_mailmap: bool,
    pub mailmap: MailmapOptions,
    pub max_groups: usize,
    pub max_subject_bytes: usize,
}

impl Default for ShortlogOptions {
    fn default() -> Self {
        Self {
            walk: RevisionWalkOptions::default(),
            paths: Vec::new(),
            groups: vec![ShortlogGroup::Author],
            include_email: false,
            sort_by_number: false,
            use_mailmap: true,
            mailmap: MailmapOptions::default(),
            max_groups: 1_000_000,
            max_subject_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// One canonical identity and its oldest-first commit subjects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortlogEntry {
    identity: Vec<u8>,
    subjects: Vec<Vec<u8>>,
}

impl ShortlogEntry {
    #[must_use]
    pub fn identity(&self) -> &[u8] {
        &self.identity
    }

    #[must_use]
    pub const fn commit_count(&self) -> usize {
        self.subjects.len()
    }

    #[must_use]
    pub fn subjects(&self) -> &[Vec<u8>] {
        &self.subjects
    }
}

impl Repository {
    /// Summarize selected commits by canonical author, committer, or trailer.
    ///
    /// A commit contributes at most once to an identical group value even if
    /// multiple requested sources produce it. Subjects within each group are
    /// oldest first. Groups are alphabetic unless count ordering is requested;
    /// count ties retain alphabetic order.
    ///
    /// # Errors
    /// Returns an error for invalid groups or paths, revision/object/mailmap
    /// failures, or graph, group-count, and aggregate-subject limits.
    pub fn shortlog(
        &self,
        include: &[ObjectId],
        exclude: &[ObjectId],
        options: &ShortlogOptions,
    ) -> Result<Vec<ShortlogEntry>> {
        validate_groups(&options.groups)?;
        let mailmap = options
            .use_mailmap
            .then(|| self.load_mailmap(&options.mailmap))
            .transpose()?;
        let log = self.log(
            include,
            exclude,
            &LogOptions {
                walk: options.walk.clone(),
                paths: options.paths.clone(),
                show_patch: false,
                ..LogOptions::default()
            },
        )?;
        let mut grouped = BTreeMap::<Vec<u8>, Vec<Vec<u8>>>::new();
        let mut subject_bytes = 0usize;
        for entry in log {
            let commit = entry.commit();
            let mut identities = BTreeSet::new();
            for group in &options.groups {
                match group {
                    ShortlogGroup::Author => identities.insert(format_signature(
                        commit.author(),
                        mailmap.as_ref(),
                        options.include_email,
                    )),
                    ShortlogGroup::Committer => identities.insert(format_signature(
                        commit.committer(),
                        mailmap.as_ref(),
                        options.include_email,
                    )),
                    ShortlogGroup::Trailer(key) => {
                        for value in matching_trailers(commit.message(), key.as_bytes()) {
                            identities.insert(format_trailer(
                                &value,
                                mailmap.as_ref(),
                                options.include_email,
                            ));
                        }
                        false
                    }
                };
            }
            if identities.is_empty() {
                continue;
            }
            let subject = subject(commit.message());
            for identity in identities {
                if !grouped.contains_key(&identity) && grouped.len() == options.max_groups {
                    return Err(Error::InvalidRepository(format!(
                        "shortlog exceeds {} groups",
                        options.max_groups
                    )));
                }
                subject_bytes = subject_bytes.checked_add(subject.len()).ok_or_else(|| {
                    Error::InvalidRepository("shortlog subject size overflow".into())
                })?;
                if subject_bytes > options.max_subject_bytes {
                    return Err(Error::InvalidRepository(format!(
                        "shortlog subjects exceed {} bytes",
                        options.max_subject_bytes
                    )));
                }
                grouped.entry(identity).or_default().push(subject.clone());
            }
        }
        let mut entries = grouped
            .into_iter()
            .map(|(identity, mut subjects)| {
                subjects.reverse();
                ShortlogEntry { identity, subjects }
            })
            .collect::<Vec<_>>();
        if options.sort_by_number {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.commit_count()));
        }
        Ok(entries)
    }
}

fn validate_groups(groups: &[ShortlogGroup]) -> Result<()> {
    if groups.is_empty() {
        return Err(Error::InvalidRepository(
            "shortlog requires at least one group".into(),
        ));
    }
    for group in groups {
        if let ShortlogGroup::Trailer(key) = group
            && (key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        {
            return Err(Error::InvalidRepository(format!(
                "invalid shortlog trailer key `{key}`"
            )));
        }
    }
    Ok(())
}

fn format_signature(
    signature: &Signature,
    mailmap: Option<&crate::Mailmap>,
    include_email: bool,
) -> Vec<u8> {
    let (name, email) = mailmap.map_or((signature.name(), signature.email()), |mailmap| {
        mailmap.map_identity(signature.name(), signature.email())
    });
    format_identity(name, email, include_email)
}

fn format_identity(name: &str, email: &str, include_email: bool) -> Vec<u8> {
    if include_email {
        format!("{name} <{email}>").into_bytes()
    } else {
        name.as_bytes().to_vec()
    }
}

fn format_trailer(value: &[u8], mailmap: Option<&crate::Mailmap>, include_email: bool) -> Vec<u8> {
    let Ok(value_text) = std::str::from_utf8(value) else {
        return value.to_vec();
    };
    let Some((name, email)) = parse_trailer_identity(value_text) else {
        return value.to_vec();
    };
    let (name, email) = mailmap.map_or((name, email), |mailmap| mailmap.map_identity(name, email));
    format_identity(name, email, include_email)
}

fn parse_trailer_identity(value: &str) -> Option<(&str, &str)> {
    let left = value.find('<')?;
    let right = value[left + 1..].find('>')? + left + 1;
    if !value[right + 1..].trim().is_empty() {
        return None;
    }
    let name = value[..left].trim();
    let email = &value[left + 1..right];
    (!name.is_empty() && !email.is_empty()).then_some((name, email))
}

fn subject(message: &[u8]) -> Vec<u8> {
    let mut message = trim_ascii(message);
    if message.starts_with(b"[PATCH")
        && let Some(end) = message.iter().position(|byte| *byte == b']')
    {
        message = trim_ascii_start(&message[end + 1..]);
    }
    let mut output = Vec::new();
    for line in message.split(|byte| *byte == b'\n') {
        let line = trim_ascii(line);
        if line.is_empty() {
            break;
        }
        if !output.is_empty() {
            output.push(b' ');
        }
        output.extend_from_slice(line);
    }
    if output.is_empty() {
        b"<none>".to_vec()
    } else {
        output
    }
}

fn matching_trailers(message: &[u8], wanted: &[u8]) -> Vec<Vec<u8>> {
    let lines = message.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let mut end = lines.len();
    while end > 0 && trim_ascii(lines[end - 1]).is_empty() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && !trim_ascii(lines[start - 1]).is_empty() {
        start -= 1;
    }
    let mut trailers = Vec::<(Vec<u8>, Vec<u8>)>::new();
    for line in &lines[start..end] {
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            let Some((_, value)) = trailers.last_mut() else {
                return Vec::new();
            };
            value.push(b' ');
            value.extend_from_slice(trim_ascii(line));
            continue;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Vec::new();
        };
        let key = trim_ascii(&line[..colon]);
        if key.is_empty()
            || !key
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Vec::new();
        }
        trailers.push((key.to_vec(), trim_ascii(&line[colon + 1..]).to_vec()));
    }
    trailers
        .into_iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value)
        .collect()
}

fn trim_ascii_start(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    value
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    value = trim_ascii_start(value);
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{ShortlogGroup, ShortlogOptions};
    use crate::{
        CommitBuilder, FileSystem, InitOptions, MemoryFileSystem, Repository, Signature, Tree,
    };

    #[test]
    fn groups_mapped_authors_and_trailers_with_git_ordering() {
        let filesystem = MemoryFileSystem::new();
        let repository =
            Repository::init(filesystem.clone(), "repo", &InitOptions::default()).unwrap();
        filesystem
            .write(
                std::path::Path::new("repo/.mailmap"),
                b"Canonical <canonical@example.com> Alias <alias@example.com>\n",
            )
            .unwrap();
        let tree = repository
            .write_tree(&Tree::new(Vec::new()).unwrap())
            .unwrap();
        let first = commit(
            &repository,
            tree,
            None,
            ("Alias", "alias@example.com"),
            b"[PATCH v2] First subject\n\nReviewed-by: Reviewer <review@example.com>\n",
            1,
        );
        let second = commit(
            &repository,
            tree,
            Some(first),
            ("Canonical", "canonical@example.com"),
            b"Second\ncontinued\n\nReviewed-by: Reviewer <review@example.com>\nReviewed-by: Reviewer <review@example.com>\n",
            2,
        );

        let result = repository
            .shortlog(
                &[second],
                &[],
                &ShortlogOptions {
                    groups: vec![
                        ShortlogGroup::Author,
                        ShortlogGroup::Trailer("reviewed-by".into()),
                    ],
                    include_email: true,
                    ..ShortlogOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].identity(), b"Canonical <canonical@example.com>");
        assert_eq!(
            result[0].subjects(),
            [b"First subject".as_slice(), b"Second continued".as_slice()]
        );
        assert_eq!(result[1].identity(), b"Reviewer <review@example.com>");
        assert_eq!(result[1].commit_count(), 2);
    }

    #[test]
    fn count_sort_is_descending_with_alphabetic_ties_and_limits() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = repository
            .write_tree(&Tree::new(Vec::new()).unwrap())
            .unwrap();
        let one = commit(&repository, tree, None, ("Zed", "z@e"), b"one\n", 1);
        let two = commit(&repository, tree, Some(one), ("Amy", "a@e"), b"two\n", 2);
        let tip = commit(&repository, tree, Some(two), ("Amy", "a@e"), b"three\n", 3);
        let result = repository
            .shortlog(
                &[tip],
                &[],
                &ShortlogOptions {
                    sort_by_number: true,
                    use_mailmap: false,
                    ..ShortlogOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result[0].identity(), b"Amy");
        assert_eq!(result[0].commit_count(), 2);
        assert_eq!(result[1].identity(), b"Zed");

        assert!(
            repository
                .shortlog(
                    &[tip],
                    &[],
                    &ShortlogOptions {
                        max_groups: 1,
                        use_mailmap: false,
                        ..ShortlogOptions::default()
                    }
                )
                .is_err()
        );
    }

    fn commit(
        repository: &Repository,
        tree: crate::ObjectId,
        parent: Option<crate::ObjectId>,
        author: (&str, &str),
        message: &[u8],
        timestamp: i64,
    ) -> crate::ObjectId {
        let author = Signature::new(author.0, author.1, timestamp, 0).unwrap();
        let committer = Signature::new("Committer", "c@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, author, committer).message(message.to_vec());
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
