//! Commit object parsing and canonical serialization.

use std::str::FromStr;

use crate::{Error, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signature {
    name: String,
    email: String,
    timestamp: i64,
    offset_minutes: i16,
    negative_zero: bool,
    timezone: [u8; 5],
}

impl Signature {
    /// Create an author or committer identity.
    ///
    /// # Errors
    /// Returns an error for unsafe identity delimiters or timezone offsets
    /// outside `-2359..=+2359`.
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        timestamp: i64,
        offset_minutes: i16,
    ) -> Result<Self> {
        let name = name.into();
        let email = email.into();
        validate_identity(&name, &email)?;
        if !(-1439..=1439).contains(&offset_minutes) {
            return Err(Error::InvalidCommit(
                "timezone offset is out of range".into(),
            ));
        }
        let absolute = offset_minutes.unsigned_abs();
        let timezone = format!(
            "{}{:02}{:02}",
            if offset_minutes < 0 { '-' } else { '+' },
            absolute / 60,
            absolute % 60
        )
        .into_bytes()
        .try_into()
        .map_err(|_| Error::InvalidCommit("invalid generated timezone".into()))?;
        Ok(Self {
            name,
            email,
            timestamp,
            offset_minutes,
            negative_zero: false,
            timezone,
        })
    }

    /// Create an identity with Git's `-0000` unknown-timezone marker.
    ///
    /// # Errors
    /// Returns an error for unsafe identity delimiters.
    pub fn with_unknown_timezone(
        name: impl Into<String>,
        email: impl Into<String>,
        timestamp: i64,
    ) -> Result<Self> {
        let mut signature = Self::new(name, email, timestamp, 0)?;
        signature.negative_zero = true;
        signature.timezone = *b"-0000";
        Ok(signature)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn email(&self) -> &str {
        &self.email
    }

    #[must_use]
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }

    #[must_use]
    pub const fn offset_minutes(&self) -> i16 {
        self.offset_minutes
    }

    #[must_use]
    pub const fn has_unknown_timezone(&self) -> bool {
        self.negative_zero
    }

    /// Encode the canonical `name <email> timestamp timezone` form used by Git.
    #[must_use]
    pub fn encode(&self) -> String {
        let timezone = std::str::from_utf8(&self.timezone).unwrap_or("+0000");
        format!(
            "{} <{}> {} {timezone}",
            self.name, self.email, self.timestamp
        )
    }

    pub(crate) fn parse(value: &[u8]) -> Result<Self> {
        let value = std::str::from_utf8(value)
            .map_err(|_| Error::InvalidCommit("identity is not UTF-8".into()))?;
        let (identity, timezone) = value
            .rsplit_once(' ')
            .ok_or_else(|| Error::InvalidCommit("identity has no timezone".into()))?;
        let (identity, timestamp) = identity
            .rsplit_once(' ')
            .ok_or_else(|| Error::InvalidCommit("identity has no timestamp".into()))?;
        let email_start = identity
            .rfind(" <")
            .ok_or_else(|| Error::InvalidCommit("identity has no email opener".into()))?;
        if !identity.ends_with('>') {
            return Err(Error::InvalidCommit("identity has no email closer".into()));
        }
        let name = &identity[..email_start];
        let email = &identity[email_start + 2..identity.len() - 1];
        if timestamp.is_empty()
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || (timestamp.len() > 1 && timestamp.starts_with('0'))
        {
            return Err(Error::InvalidCommit("invalid identity timestamp".into()));
        }
        let timestamp = timestamp
            .parse::<i64>()
            .map_err(|_| Error::InvalidCommit("invalid identity timestamp".into()))?;
        let bytes = timezone.as_bytes();
        if bytes.len() != 5
            || !matches!(bytes[0], b'+' | b'-')
            || !bytes[1..].iter().all(u8::is_ascii_digit)
        {
            return Err(Error::InvalidCommit("invalid identity timezone".into()));
        }
        let hours = i16::from(bytes[1] - b'0') * 10 + i16::from(bytes[2] - b'0');
        let minutes = i16::from(bytes[3] - b'0') * 10 + i16::from(bytes[4] - b'0');
        let mut offset = hours * 60 + minutes;
        if bytes[0] == b'-' {
            offset = -offset;
        }
        validate_identity(name, email)?;
        Ok(Self {
            name: name.to_owned(),
            email: email.to_owned(),
            timestamp,
            offset_minutes: offset,
            negative_zero: bytes[0] == b'-' && offset == 0,
            timezone: bytes.try_into().expect("validated timezone length"),
        })
    }
}

fn validate_identity(name: &str, email: &str) -> Result<()> {
    if name.is_empty()
        || email.is_empty()
        || name.contains(['<', '>', '\n', '\r', '\0'])
        || email.contains(['<', '>', '\n', '\r', '\0'])
    {
        return Err(Error::InvalidCommit(
            "invalid identity name or email".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtraHeader {
    name: Vec<u8>,
    value: Vec<u8>,
}

impl ExtraHeader {
    /// Create an additional commit header such as `encoding` or `gpgsig`.
    ///
    /// Embedded newlines in `value` must be followed by a space, as required
    /// for Git continuation lines.
    ///
    /// # Errors
    /// Returns an error for an invalid header name or continuation.
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<Self> {
        let name = name.into();
        let value = value.into();
        if name.is_empty()
            || name
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || *byte == 0)
            || value.contains(&0)
            || value
                .split(|byte| *byte == b'\n')
                .skip(1)
                .any(|line| !line.starts_with(b" "))
        {
            return Err(Error::InvalidCommit("invalid extra header".into()));
        }
        Ok(Self { name, value })
    }

    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Commit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    committer: Signature,
    extra_headers: Vec<ExtraHeader>,
    message: Vec<u8>,
}

impl Commit {
    #[must_use]
    pub const fn tree(&self) -> ObjectId {
        self.tree
    }

    #[must_use]
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    #[must_use]
    pub const fn author(&self) -> &Signature {
        &self.author
    }

    #[must_use]
    pub const fn committer(&self) -> &Signature {
        &self.committer
    }

    #[must_use]
    pub fn extra_headers(&self) -> &[ExtraHeader] {
        &self.extra_headers
    }

    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// Parse a commit body, preserving its message and unknown headers.
    ///
    /// # Errors
    /// Returns an error for malformed, missing, or duplicate required headers.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let separator = data
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| Error::InvalidCommit("missing header/message separator".into()))?;
        let headers = parse_headers(&data[..separator])?;
        let message = data[separator + 2..].to_vec();

        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut extra_headers = Vec::new();
        for header in headers {
            match header.name.as_slice() {
                b"tree" if tree.is_none() => tree = Some(parse_id(&header.value, "tree")?),
                b"tree" => return Err(Error::InvalidCommit("duplicate tree header".into())),
                b"parent" => parents.push(parse_id(&header.value, "parent")?),
                b"author" if author.is_none() => author = Some(Signature::parse(&header.value)?),
                b"author" => return Err(Error::InvalidCommit("duplicate author header".into())),
                b"committer" if committer.is_none() => {
                    committer = Some(Signature::parse(&header.value)?);
                }
                b"committer" => {
                    return Err(Error::InvalidCommit("duplicate committer header".into()));
                }
                _ => extra_headers.push(header),
            }
        }
        Ok(Self {
            tree: tree.ok_or_else(|| Error::InvalidCommit("missing tree header".into()))?,
            parents,
            author: author.ok_or_else(|| Error::InvalidCommit("missing author header".into()))?,
            committer: committer
                .ok_or_else(|| Error::InvalidCommit("missing committer header".into()))?,
            extra_headers,
            message,
        })
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(256 + self.message.len());
        push_header(&mut data, b"tree", self.tree.to_string().as_bytes());
        for parent in &self.parents {
            push_header(&mut data, b"parent", parent.to_string().as_bytes());
        }
        push_header(&mut data, b"author", self.author.encode().as_bytes());
        push_header(&mut data, b"committer", self.committer.encode().as_bytes());
        for header in &self.extra_headers {
            push_header(&mut data, &header.name, &header.value);
        }
        data.push(b'\n');
        data.extend_from_slice(&self.message);
        data
    }
}

pub(crate) fn parse_commit_links(data: &[u8]) -> Result<(ObjectId, Vec<ObjectId>)> {
    let headers = data
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty());
    let mut tree = None;
    let mut parents = Vec::new();
    for line in headers {
        if let Some(value) = line.strip_prefix(b"tree ") {
            if tree.is_some() {
                return Err(Error::InvalidCommit("duplicate tree header".into()));
            }
            tree = Some(parse_id(value, "tree")?);
        } else if let Some(value) = line.strip_prefix(b"parent ") {
            parents.push(parse_id(value, "parent")?);
        }
    }
    Ok((
        tree.ok_or_else(|| Error::InvalidCommit("missing tree header".into()))?,
        parents,
    ))
}

#[derive(Clone, Debug)]
pub struct CommitBuilder {
    commit: Commit,
}

impl CommitBuilder {
    #[must_use]
    pub fn new(tree: ObjectId, author: Signature, committer: Signature) -> Self {
        Self {
            commit: Commit {
                tree,
                parents: Vec::new(),
                author,
                committer,
                extra_headers: Vec::new(),
                message: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn parent(mut self, parent: ObjectId) -> Self {
        self.commit.parents.push(parent);
        self
    }

    #[must_use]
    pub fn message(mut self, message: impl Into<Vec<u8>>) -> Self {
        self.commit.message = message.into();
        self
    }

    #[must_use]
    pub fn extra_header(mut self, header: ExtraHeader) -> Self {
        self.commit.extra_headers.push(header);
        self
    }

    #[must_use]
    pub fn build(self) -> Commit {
        self.commit
    }
}

impl Repository {
    /// Store a canonical commit object.
    ///
    /// # Errors
    /// Returns an error when object storage fails.
    pub fn write_commit(&self, commit: &Commit) -> Result<ObjectId> {
        self.write_object(ObjectKind::Commit, &commit.encode())
    }

    /// Read and parse a loose commit object.
    ///
    /// # Errors
    /// Returns an error for a missing, corrupt, oversized, non-commit, or
    /// malformed object.
    pub fn read_commit(&self, id: ObjectId, max_size: usize) -> Result<Commit> {
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Commit {
            return Err(Error::InvalidCommit(format!("object {id} is not a commit")));
        }
        Commit::parse(object.data())
    }
}

fn parse_headers(data: &[u8]) -> Result<Vec<ExtraHeader>> {
    let mut headers: Vec<ExtraHeader> = Vec::new();
    for line in data.split(|byte| *byte == b'\n') {
        if line.starts_with(b" ") {
            let previous = headers
                .last_mut()
                .ok_or_else(|| Error::InvalidCommit("orphan continuation line".into()))?;
            previous.value.push(b'\n');
            previous.value.extend_from_slice(line);
            continue;
        }
        let space = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::InvalidCommit("header has no value separator".into()))?;
        headers.push(ExtraHeader::new(
            line[..space].to_vec(),
            line[space + 1..].to_vec(),
        )?);
    }
    Ok(headers)
}

fn parse_id(value: &[u8], header: &str) -> Result<ObjectId> {
    let text = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidCommit(format!("non-UTF-8 {header} object ID")))?;
    ObjectId::from_str(text)
        .map_err(|_| Error::InvalidCommit(format!("invalid {header} object ID")))
}

fn push_header(data: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    data.extend_from_slice(name);
    data.push(b' ');
    data.extend_from_slice(value);
    data.push(b'\n');
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{InitOptions, MemoryFileSystem, Tree};

    fn signature() -> Signature {
        Signature::new("A U Thor", "author@example.com", 1_700_000_000, 90).unwrap()
    }

    #[test]
    fn signature_round_trips_positive_and_negative_offsets() {
        for offset in [90, -330, 0] {
            let signature = Signature::new("Name", "a@example.com", 123, offset).unwrap();
            assert_eq!(
                Signature::parse(signature.encode().as_bytes()).unwrap(),
                signature
            );
        }
        let unknown = Signature::with_unknown_timezone("Name", "a@example.com", 123).unwrap();
        assert_eq!(unknown.encode(), "Name <a@example.com> 123 -0000");
        assert_eq!(
            Signature::parse(unknown.encode().as_bytes()).unwrap(),
            unknown
        );
    }

    #[test]
    fn commit_links_do_not_require_utf8_identities() {
        let tree = ObjectId::compute(ObjectKind::Tree, b"");
        let parent = ObjectId::compute(ObjectKind::Commit, b"parent");
        let data = format!("tree {tree}\nparent {parent}\nauthor ").into_bytes();
        let data = [
            data,
            vec![0xff],
            b" <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nmessage".to_vec(),
        ]
        .concat();

        assert!(Commit::parse(&data).is_err());
        assert_eq!(parse_commit_links(&data).unwrap(), (tree, vec![parent]));
    }

    #[test]
    fn commit_round_trips_merges_binary_messages_and_multiline_headers() {
        let tree = ObjectId::from_str("1111111111111111111111111111111111111111").unwrap();
        let first = ObjectId::from_str("2222222222222222222222222222222222222222").unwrap();
        let second = ObjectId::from_str("3333333333333333333333333333333333333333").unwrap();
        let signature = signature();
        let commit = CommitBuilder::new(tree, signature.clone(), signature)
            .parent(first)
            .parent(second)
            .extra_header(
                ExtraHeader::new("gpgsig", b"-----BEGIN\n continuation".to_vec()).unwrap(),
            )
            .message(b"subject\n\nnon-utf8: \xff\n".to_vec())
            .build();
        assert_eq!(Commit::parse(&commit.encode()).unwrap(), commit);
    }

    #[test]
    fn writes_a_stable_root_commit_and_reads_it() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = Signature::new("Test", "test@example.com", 0, 0).unwrap();
        let commit = CommitBuilder::new(tree, identity.clone(), identity)
            .message(b"initial\n".to_vec())
            .build();
        let id = repository.write_commit(&commit).unwrap();
        assert_eq!(id.to_string(), "27a4ab2322732fe357cc1e9466f62de7ad55106b");
        assert_eq!(repository.read_commit(id, 4096).unwrap(), commit);
    }

    #[test]
    fn rejects_missing_headers_bad_timezones_and_orphan_continuations() {
        assert!(
            Commit::parse(b"tree 1111111111111111111111111111111111111111\n\nmessage").is_err()
        );
        let historical = Signature::parse(b"Name <a@example.com> 123 +9999").unwrap();
        assert_eq!(historical.encode(), "Name <a@example.com> 123 +9999");
        assert!(Signature::parse(b"Name <a@example.com> 0123 +0000").is_err());
        assert!(Commit::parse(b" continuation\n\nmessage").is_err());
    }
}
