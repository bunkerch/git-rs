//! Stateful, storage-agnostic application of email patch series.

use std::path::Path;

use crate::status::worktree_path;
use crate::{
    ApplyOptions, CheckoutOptions, CommitOptions, Error, ObjectId, ReferenceTarget, RemoveOptions,
    Repository, Result, Signature, StatusOptions,
};

const STATE_PATH: &str = "rebase-apply/git-rs-state";
const STATE_DIR: &str = "rebase-apply";
const MAGIC: &[u8; 8] = b"GRAM0001";

/// Mail parsing, patch application, commit, and resource policy for `am`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmOptions {
    pub apply: ApplyOptions,
    pub commit: CommitOptions,
    pub max_mails: usize,
    pub max_mail_bytes: usize,
    pub max_total_bytes: usize,
    pub max_header_bytes: usize,
}

impl Default for AmOptions {
    fn default() -> Self {
        Self {
            apply: ApplyOptions::default(),
            commit: CommitOptions::default(),
            max_mails: 100_000,
            max_mail_bytes: 1024 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_header_bytes: 1024 * 1024,
        }
    }
}

/// Current persistent AM position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmState {
    pub next: usize,
    pub total: usize,
}

/// Commits created by one start/continue/skip call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AmProgress {
    pub commits: Vec<ObjectId>,
    pub completed: bool,
}

#[derive(Clone, Debug)]
enum OriginalHead {
    Symbolic(String),
    Detached,
}

#[derive(Clone, Debug)]
struct StoredState {
    original_head: OriginalHead,
    original: Option<ObjectId>,
    next: usize,
    mails: Vec<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct ParsedMail {
    author: Signature,
    message: Vec<u8>,
    patch: Vec<u8>,
}

impl Repository {
    /// Start and run an email patch series until completion or the first conflict.
    ///
    /// Repository state must be clean. On a patch or commit failure, persistent
    /// state remains available to [`Self::continue_am`], [`Self::skip_am`], or
    /// [`Self::abort_am`]. All mail bytes are stored through the filesystem adapter.
    ///
    /// # Errors
    /// Returns an error for an active operation, dirty tracked state, malformed
    /// or oversized mail, patch conflicts, commit failures, or storage failures.
    pub fn am(
        &self,
        mails: &[Vec<u8>],
        committer: &Signature,
        options: &AmOptions,
    ) -> Result<AmProgress> {
        if mails.is_empty() || mails.len() > options.max_mails {
            return Err(Error::InvalidRepository("invalid AM mail count".into()));
        }
        if self.filesystem().exists(&self.git_path(STATE_DIR))? {
            return Err(Error::InvalidRepository(
                "an AM or rebase is already active".into(),
            ));
        }
        if !self
            .status(&StatusOptions {
                include_untracked: false,
                max_object_size: options.commit.max_object_size,
            })?
            .is_clean()
        {
            return Err(Error::InvalidRepository(
                "AM requires a clean index and tracked worktree".into(),
            ));
        }
        validate_mails(mails, options)?;
        let head = self.read_reference("HEAD")?;
        let original_head = match head.target() {
            ReferenceTarget::Symbolic(name) => OriginalHead::Symbolic(name.as_str().to_owned()),
            ReferenceTarget::Direct(_) => OriginalHead::Detached,
        };
        let original = match self.resolve_reference("HEAD") {
            Ok(id) => Some(id),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let state = StoredState {
            original_head,
            original,
            next: 0,
            mails: mails.to_vec(),
        };
        self.filesystem()
            .create_dir_all(&self.git_path(STATE_DIR))?;
        self.write_am_state(&state)?;
        self.run_am(state, committer, options)
    }

    /// Commit the currently staged conflict resolution and resume the series.
    ///
    /// # Errors
    /// Returns an error when no AM is active, the current mail/state is invalid,
    /// the index is unresolved, the commit fails, or a later patch conflicts.
    pub fn continue_am(&self, committer: &Signature, options: &AmOptions) -> Result<AmProgress> {
        let mut state = self.read_am_state()?;
        if state.next == state.mails.len() {
            self.clear_am_state()?;
            return Ok(AmProgress {
                commits: Vec::new(),
                completed: true,
            });
        }
        let mail = parse_mail(&state.mails[state.next], options)?;
        let commit = self.commit_index_during_am(
            &mail.message,
            &mail.author,
            committer,
            &am_commit_options(options),
        )?;
        state.next += 1;
        self.write_am_state(&state)?;
        let mut progress = self.run_am(state, committer, options)?;
        progress.commits.insert(0, commit);
        Ok(progress)
    }

    /// Discard the failed mail's worktree/index changes and resume at the next mail.
    ///
    /// # Errors
    /// Returns an error when no AM is active, checkout/state storage fails, or a
    /// later mail cannot be applied.
    pub fn skip_am(&self, committer: &Signature, options: &AmOptions) -> Result<AmProgress> {
        let mut state = self.read_am_state()?;
        if state.next == state.mails.len() {
            self.clear_am_state()?;
            return Ok(AmProgress {
                commits: Vec::new(),
                completed: true,
            });
        }
        if let Ok(current) = self.resolve_reference("HEAD") {
            let tree = self
                .read_commit(current, options.commit.max_object_size)?
                .tree();
            self.checkout_tree(
                tree,
                &CheckoutOptions {
                    force: true,
                    max_object_size: options.commit.max_object_size,
                },
            )?;
        }
        state.next += 1;
        self.write_am_state(&state)?;
        self.run_am(state, committer, options)
    }

    /// Restore the original tip, index, and tracked worktree and remove AM state.
    ///
    /// # Errors
    /// Returns an error when no AM is active, HEAD changed to another branch,
    /// restoration/ref deletion fails, or state cleanup fails.
    pub fn abort_am(&self, committer: &Signature, options: &AmOptions) -> Result<()> {
        let state = self.read_am_state()?;
        self.verify_am_head(&state)?;
        match state.original {
            Some(original) => self.reset(
                original,
                &crate::ResetOptions {
                    mode: crate::ResetMode::Hard,
                    max_object_size: options.commit.max_object_size,
                },
                committer,
            )?,
            None => self.abort_unborn_am(&state, options)?,
        }
        self.clear_am_state()
    }

    /// Inspect the current series position without changing it.
    ///
    /// # Errors
    /// Returns an error if no AM is active or its state is corrupt.
    pub fn am_state(&self) -> Result<AmState> {
        let state = self.read_am_state()?;
        Ok(AmState {
            next: state.next,
            total: state.mails.len(),
        })
    }

    fn run_am(
        &self,
        mut state: StoredState,
        committer: &Signature,
        options: &AmOptions,
    ) -> Result<AmProgress> {
        let mut commits = Vec::new();
        while state.next < state.mails.len() {
            let mail = parse_mail(&state.mails[state.next], options)?;
            let mut apply = options.apply.clone();
            apply.check = false;
            apply.index = true;
            apply.reverse = false;
            self.apply_patch(&mail.patch, &apply)?;
            let commit = self.commit_index_during_am(
                &mail.message,
                &mail.author,
                committer,
                &am_commit_options(options),
            )?;
            commits.push(commit);
            state.next += 1;
            self.write_am_state(&state)?;
        }
        self.clear_am_state()?;
        Ok(AmProgress {
            commits,
            completed: true,
        })
    }

    fn abort_unborn_am(&self, state: &StoredState, options: &AmOptions) -> Result<()> {
        let head = self.read_reference("HEAD")?;
        let OriginalHead::Symbolic(original_name) = &state.original_head else {
            return Err(Error::InvalidRepository(
                "unborn AM state cannot have detached HEAD".into(),
            ));
        };
        match head.target() {
            ReferenceTarget::Symbolic(actual) if actual.as_str() == original_name => {}
            _ => return Err(Error::ReferenceConflict("HEAD".into())),
        }
        let index = self.read_index()?;
        if !index.entries().is_empty() {
            let paths = index
                .entries()
                .iter()
                .map(|entry| worktree_path(entry.path()))
                .collect::<Result<Vec<_>>>()?;
            self.remove(
                &paths,
                &RemoveOptions {
                    force: true,
                    include_sparse: true,
                    ignore_unmatched: true,
                    max_object_size: options.commit.max_object_size,
                    ..RemoveOptions::default()
                },
            )?;
        }
        if let Ok(current) = self.resolve_reference("HEAD") {
            let name = crate::ReferenceName::new(original_name.clone())?;
            self.delete_reference(&name, current)?;
        }
        Ok(())
    }

    fn verify_am_head(&self, state: &StoredState) -> Result<()> {
        let head = self.read_reference("HEAD")?;
        match (&state.original_head, head.target()) {
            (OriginalHead::Symbolic(expected), ReferenceTarget::Symbolic(actual))
                if expected == actual.as_str() =>
            {
                Ok(())
            }
            (OriginalHead::Detached, ReferenceTarget::Direct(_)) => Ok(()),
            _ => Err(Error::ReferenceConflict("HEAD".into())),
        }
    }

    fn write_am_state(&self, state: &StoredState) -> Result<()> {
        self.write_atomic(Path::new(STATE_PATH), &encode_state(state)?)
    }

    fn read_am_state(&self) -> Result<StoredState> {
        decode_state(&self.read_git_file(STATE_PATH)?)
    }

    fn clear_am_state(&self) -> Result<()> {
        self.filesystem().remove_file(&self.git_path(STATE_PATH))?;
        self.filesystem().remove_dir(&self.git_path(STATE_DIR))
    }
}

fn validate_mails(mails: &[Vec<u8>], options: &AmOptions) -> Result<()> {
    let mut total = 0usize;
    for mail in mails {
        if mail.len() > options.max_mail_bytes {
            return Err(Error::ObjectTooLarge {
                declared: mail.len() as u64,
                limit: options.max_mail_bytes,
            });
        }
        total = total
            .checked_add(mail.len())
            .ok_or_else(|| Error::InvalidRepository("AM mail size overflow".into()))?;
        if total > options.max_total_bytes {
            return Err(Error::ObjectTooLarge {
                declared: total as u64,
                limit: options.max_total_bytes,
            });
        }
        parse_mail(mail, options)?;
    }
    Ok(())
}

fn am_commit_options(options: &AmOptions) -> CommitOptions {
    CommitOptions {
        amend: false,
        ..options.commit.clone()
    }
}

fn parse_mail(mail: &[u8], options: &AmOptions) -> Result<ParsedMail> {
    if mail.len() > options.max_mail_bytes {
        return Err(Error::ObjectTooLarge {
            declared: mail.len() as u64,
            limit: options.max_mail_bytes,
        });
    }
    let header_start = if mail.starts_with(b"From ") {
        mail.iter()
            .position(|byte| *byte == b'\n')
            .map_or(mail.len(), |position| position + 1)
    } else {
        0
    };
    let relative_end = mail[header_start..]
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .ok_or_else(|| Error::InvalidRepository("mail has no header separator".into()))?;
    let header_end = header_start + relative_end;
    if header_end - header_start > options.max_header_bytes {
        return Err(Error::ObjectTooLarge {
            declared: (header_end - header_start) as u64,
            limit: options.max_header_bytes,
        });
    }
    let headers = parse_headers(&mail[header_start..header_end])?;
    let from = header(&headers, b"from")?;
    let subject = decode_header(header(&headers, b"subject")?)?;
    let date = header(&headers, b"date")?;
    let (name, email) = parse_address(&decode_header(from)?)?;
    let (timestamp, offset, unknown) = parse_date(date)?;
    let author = if unknown {
        Signature::with_unknown_timezone(name, email, timestamp)?
    } else {
        Signature::new(name, email, timestamp, offset)?
    };
    let subject = strip_patch_prefix(trim_ascii(&subject));
    let body = &mail[header_end + 2..];
    let patch_start = find_line(body, b"diff --git ")
        .ok_or_else(|| Error::InvalidRepository("mail contains no Git patch".into()))?;
    let before_patch = &body[..patch_start];
    let log_end = find_separator(before_patch).unwrap_or(before_patch.len());
    let log = trim_blank_lines(&before_patch[..log_end]);
    let mut message = subject.to_vec();
    if !log.is_empty() {
        message.extend_from_slice(b"\n\n");
        message.extend_from_slice(log);
    }
    message.push(b'\n');
    Ok(ParsedMail {
        author,
        message,
        patch: body[patch_start..].to_vec(),
    })
}

fn parse_headers(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in data.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if matches!(line.first(), Some(b' ' | b'\t')) {
            let (_, value) = headers
                .last_mut()
                .ok_or_else(|| Error::InvalidRepository("orphan folded mail header".into()))?;
            value.push(b' ');
            value.extend_from_slice(trim_ascii(line));
            continue;
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| Error::InvalidRepository("malformed mail header".into()))?;
        let mut name = line[..colon].to_vec();
        name.make_ascii_lowercase();
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err(Error::InvalidRepository("invalid mail header name".into()));
        }
        headers.push((name, trim_ascii(&line[colon + 1..]).to_vec()));
    }
    Ok(headers)
}

fn header<'a>(headers: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Result<&'a [u8]> {
    headers
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_slice())
        .ok_or_else(|| {
            Error::InvalidRepository(format!(
                "mail lacks {} header",
                String::from_utf8_lossy(name)
            ))
        })
}

fn parse_address(value: &[u8]) -> Result<(String, String)> {
    let text = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("mail author is not UTF-8".into()))?;
    let open = text
        .rfind('<')
        .ok_or_else(|| Error::InvalidRepository("mail From lacks <email>".into()))?;
    let close = text[open..]
        .find('>')
        .map(|value| open + value)
        .ok_or_else(|| Error::InvalidRepository("mail From lacks closing >".into()))?;
    if !text[close + 1..].trim().is_empty() {
        return Err(Error::InvalidRepository(
            "unexpected data after mail address".into(),
        ));
    }
    let mut name = text[..open].trim().to_owned();
    if name.starts_with('"') && name.ends_with('"') && name.len() >= 2 {
        name = unquote(&name[1..name.len() - 1])?;
    }
    Ok((name, text[open + 1..close].to_owned()))
}

fn parse_date(value: &[u8]) -> Result<(i64, i16, bool)> {
    let text = std::str::from_utf8(value)
        .map_err(|_| Error::InvalidRepository("mail date is not ASCII".into()))?;
    let text = text.rsplit_once(',').map_or(text, |(_, rest)| rest).trim();
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 5 {
        return Err(Error::InvalidRepository("unsupported mail date".into()));
    }
    let day = parts[0]
        .parse::<u32>()
        .map_err(|_| Error::InvalidRepository("invalid mail day".into()))?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == parts[1])
    .map(|month| month + 1)
    .ok_or_else(|| Error::InvalidRepository("invalid mail month".into()))?;
    let month = u32::try_from(month).expect("month table has twelve entries");
    let year = parts[2]
        .parse::<i64>()
        .map_err(|_| Error::InvalidRepository("invalid mail year".into()))?;
    let clock = parts[3].split(':').collect::<Vec<_>>();
    if clock.len() != 3 {
        return Err(Error::InvalidRepository("invalid mail time".into()));
    }
    let hour = clock[0]
        .parse::<u32>()
        .map_err(|_| Error::InvalidRepository("invalid mail hour".into()))?;
    let minute = clock[1]
        .parse::<u32>()
        .map_err(|_| Error::InvalidRepository("invalid mail minute".into()))?;
    let second = clock[2]
        .parse::<u32>()
        .map_err(|_| Error::InvalidRepository("invalid mail second".into()))?;
    let zone = parts[4].as_bytes();
    if zone.len() != 5
        || !matches!(zone[0], b'+' | b'-')
        || !zone[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(Error::InvalidRepository("invalid mail timezone".into()));
    }
    let zone_hour = i16::from(zone[1] - b'0') * 10 + i16::from(zone[2] - b'0');
    let zone_minute = i16::from(zone[3] - b'0') * 10 + i16::from(zone[4] - b'0');
    if day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
        || zone_hour > 23
        || zone_minute > 59
    {
        return Err(Error::InvalidRepository("mail date is out of range".into()));
    }
    let mut offset = zone_hour * 60 + zone_minute;
    if zone[0] == b'-' {
        offset = -offset;
    }
    let timestamp = days_from_civil(year, month, day)
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(i64::from(hour * 3600 + minute * 60 + second.min(59))))
        .and_then(|value| value.checked_sub(i64::from(offset) * 60))
        .ok_or_else(|| Error::InvalidRepository("mail date overflows timestamp".into()))?;
    Ok((timestamp, offset, zone == b"-0000"))
}

fn decode_header(value: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = value[cursor..].windows(2).position(|part| part == b"=?") {
        let start = cursor + relative;
        output.extend_from_slice(&value[cursor..start]);
        let end = value[start + 2..]
            .windows(2)
            .position(|part| part == b"?=")
            .map(|value| start + 2 + value)
            .ok_or_else(|| Error::InvalidRepository("unterminated encoded mail header".into()))?;
        let word = &value[start + 2..end];
        let fields = word.splitn(3, |byte| *byte == b'?').collect::<Vec<_>>();
        if fields.len() != 3 || !fields[0].eq_ignore_ascii_case(b"utf-8") {
            return Err(Error::InvalidRepository(
                "unsupported encoded mail header".into(),
            ));
        }
        match fields[1] {
            encoding if encoding.eq_ignore_ascii_case(b"b") => {
                output.extend_from_slice(&decode_base64(fields[2])?);
            }
            encoding if encoding.eq_ignore_ascii_case(b"q") => {
                output.extend_from_slice(&decode_q(fields[2])?);
            }
            _ => {
                return Err(Error::InvalidRepository(
                    "unsupported mail header encoding".into(),
                ));
            }
        }
        cursor = end + 2;
        if value[cursor..].starts_with(b" =?") {
            cursor += 1;
        }
    }
    output.extend_from_slice(&value[cursor..]);
    Ok(output)
}

fn decode_base64(data: &[u8]) -> Result<Vec<u8>> {
    if !data.len().is_multiple_of(4) {
        return Err(Error::InvalidRepository(
            "invalid Base64 mail header".into(),
        ));
    }
    let mut output = Vec::with_capacity(data.len() / 4 * 3);
    let chunk_count = data.len() / 4;
    for (index, chunk) in data.chunks_exact(4).enumerate() {
        if (chunk[2] == b'=' && chunk[3] != b'=')
            || (chunk.contains(&b'=') && index + 1 != chunk_count)
        {
            return Err(Error::InvalidRepository(
                "invalid Base64 mail header padding".into(),
            ));
        }
        let values = [
            base64_value(chunk[0]),
            base64_value(chunk[1]),
            base64_value(chunk[2]),
            base64_value(chunk[3]),
        ];
        if values[0].is_none() || values[1].is_none() {
            return Err(Error::InvalidRepository(
                "invalid Base64 mail header".into(),
            ));
        }
        let one = values[0].unwrap();
        let two = values[1].unwrap();
        output.push((one << 2) | (two >> 4));
        if chunk[2] != b'=' {
            let three = values[2]
                .ok_or_else(|| Error::InvalidRepository("invalid Base64 mail header".into()))?;
            output.push((two << 4) | (three >> 2));
            if chunk[3] != b'=' {
                let four = values[3]
                    .ok_or_else(|| Error::InvalidRepository("invalid Base64 mail header".into()))?;
                output.push((three << 6) | four);
            }
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        b'=' => Some(0),
        _ => None,
    }
}

fn decode_q(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < data.len() {
        match data[cursor] {
            b'_' => output.push(b' '),
            b'=' if cursor + 2 < data.len() => {
                let text = std::str::from_utf8(&data[cursor + 1..cursor + 3])
                    .map_err(|_| Error::InvalidRepository("invalid quoted mail header".into()))?;
                output.push(
                    u8::from_str_radix(text, 16).map_err(|_| {
                        Error::InvalidRepository("invalid quoted mail header".into())
                    })?,
                );
                cursor += 2;
            }
            b'=' => {
                return Err(Error::InvalidRepository(
                    "truncated quoted mail header".into(),
                ));
            }
            byte => output.push(byte),
        }
        cursor += 1;
    }
    Ok(output)
}

fn encode_state(state: &StoredState) -> Result<Vec<u8>> {
    let mut output = MAGIC.to_vec();
    output.push(match state.original_head {
        OriginalHead::Symbolic(_) => 1,
        OriginalHead::Detached => 2,
    });
    let name = match &state.original_head {
        OriginalHead::Symbolic(name) => name.as_bytes(),
        OriginalHead::Detached => &[],
    };
    push_bytes(&mut output, name)?;
    output.extend_from_slice(state.original.unwrap_or_else(ObjectId::null).as_bytes());
    push_u64(&mut output, state.next)?;
    push_u64(&mut output, state.mails.len())?;
    for mail in &state.mails {
        push_bytes(&mut output, mail)?;
    }
    Ok(output)
}

fn decode_state(data: &[u8]) -> Result<StoredState> {
    if data.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(Error::InvalidRepository(
            "invalid AM state signature".into(),
        ));
    }
    let mut cursor = MAGIC.len();
    let kind = take(data, &mut cursor, 1)?[0];
    let name = take_bytes(data, &mut cursor)?;
    let original_head = match kind {
        1 => OriginalHead::Symbolic(
            String::from_utf8(name)
                .map_err(|_| Error::InvalidRepository("AM branch is not UTF-8".into()))?,
        ),
        2 if name.is_empty() => OriginalHead::Detached,
        _ => return Err(Error::InvalidRepository("invalid AM HEAD state".into())),
    };
    let id = take(data, &mut cursor, ObjectId::LENGTH)?;
    let mut bytes = [0; ObjectId::LENGTH];
    bytes.copy_from_slice(id);
    let id = ObjectId::from_bytes(bytes);
    let original = (!id.is_null()).then_some(id);
    let next = take_usize(data, &mut cursor)?;
    let count = take_usize(data, &mut cursor)?;
    let mut mails = Vec::with_capacity(count.min(1_000_000));
    for _ in 0..count {
        mails.push(take_bytes(data, &mut cursor)?);
    }
    if cursor != data.len() || next > mails.len() {
        return Err(Error::InvalidRepository("invalid AM state position".into()));
    }
    Ok(StoredState {
        original_head,
        original,
        next,
        mails,
    })
}

fn push_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    push_u64(output, value.len())?;
    output.extend_from_slice(value);
    Ok(())
}
fn push_u64(output: &mut Vec<u8>, value: usize) -> Result<()> {
    output.extend_from_slice(
        &u64::try_from(value)
            .map_err(|_| Error::InvalidRepository("AM state length overflow".into()))?
            .to_be_bytes(),
    );
    Ok(())
}
fn take<'a>(data: &'a [u8], cursor: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(count)
        .ok_or_else(|| Error::InvalidRepository("AM state offset overflow".into()))?;
    let value = data
        .get(*cursor..end)
        .ok_or_else(|| Error::InvalidRepository("truncated AM state".into()))?;
    *cursor = end;
    Ok(value)
}
fn take_usize(data: &[u8], cursor: &mut usize) -> Result<usize> {
    let bytes: [u8; 8] = take(data, cursor, 8)?
        .try_into()
        .expect("slice length checked");
    usize::try_from(u64::from_be_bytes(bytes))
        .map_err(|_| Error::InvalidRepository("AM state length overflows usize".into()))
}
fn take_bytes(data: &[u8], cursor: &mut usize) -> Result<Vec<u8>> {
    let count = take_usize(data, cursor)?;
    Ok(take(data, cursor, count)?.to_vec())
}

fn strip_patch_prefix(subject: &[u8]) -> &[u8] {
    if subject.first() == Some(&b'[')
        && let Some(end) = subject.iter().position(|byte| *byte == b']')
        && subject[1..end]
            .windows(5)
            .any(|value| value.eq_ignore_ascii_case(b"patch"))
    {
        return trim_ascii(&subject[end + 1..]);
    }
    subject
}
fn find_line(data: &[u8], prefix: &[u8]) -> Option<usize> {
    if data.starts_with(prefix) {
        return Some(0);
    }
    data.windows(prefix.len() + 1)
        .position(|value| value[0] == b'\n' && value[1..] == *prefix)
        .map(|value| value + 1)
}
fn find_separator(data: &[u8]) -> Option<usize> {
    data.windows(5)
        .position(|value| value == b"\n---\n")
        .map(|value| value + 1)
        .or_else(|| data.starts_with(b"---\n").then_some(0))
}
fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}
fn trim_blank_lines(value: &[u8]) -> &[u8] {
    trim_ascii(value)
}
fn unquote(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            output.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        return Err(Error::InvalidRepository("truncated quoted author".into()));
    }
    Ok(output)
}
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        CommitBuilder, EntryMode, FileSystem, FormatPatchOptions, InitOptions, MemoryFileSystem,
        ObjectKind, PreviousValue, ReferenceName, Tree, TreeEntry,
    };

    #[test]
    fn applies_generated_series_as_commits_with_mail_authors() {
        let (repository, fs, base, tip, committer) = fixture();
        let mails = repository
            .format_patches(&[tip], &[base], &FormatPatchOptions::default())
            .unwrap()
            .into_iter()
            .map(crate::FormatPatch::into_data)
            .collect::<Vec<_>>();
        let progress = repository
            .am(&mails, &committer, &AmOptions::default())
            .unwrap();
        assert!(progress.completed);
        assert_eq!(progress.commits.len(), 2);
        assert!(matches!(repository.am_state(), Err(Error::NotFound(_))));
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"two\n");
        assert_eq!(fs.read(Path::new("repo/second")).unwrap(), b"added\n");
        let first = repository
            .read_commit(progress.commits[0], 1024 * 1024)
            .unwrap();
        assert_eq!(first.author().name(), "Mail Author");
        assert_eq!(first.author().timestamp(), 200);
        assert_eq!(first.committer(), &committer);
        assert_eq!(first.message(), b"change file\n");
    }

    #[test]
    fn conflict_can_be_resolved_and_continued() {
        let (repository, fs, base, tip, committer) = fixture();
        let mut mails = repository
            .format_patches(&[tip], &[base], &FormatPatchOptions::default())
            .unwrap()
            .into_iter()
            .map(crate::FormatPatch::into_data)
            .collect::<Vec<_>>();
        let position = mails[0]
            .windows(5)
            .position(|value| value == b"-one\n")
            .unwrap();
        mails[0][position + 1..position + 4].copy_from_slice(b"bad");
        assert!(
            repository
                .am(&mails, &committer, &AmOptions::default())
                .is_err()
        );
        assert_eq!(
            repository.am_state().unwrap(),
            AmState { next: 0, total: 2 }
        );
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"one\n");

        fs.write(Path::new("repo/file"), b"two\n").unwrap();
        repository.add("file").unwrap();
        let progress = repository
            .continue_am(&committer, &AmOptions::default())
            .unwrap();
        assert_eq!(progress.commits.len(), 2);
        assert_eq!(fs.read(Path::new("repo/second")).unwrap(), b"added\n");
    }

    #[test]
    fn abort_restores_original_tip_index_and_worktree() {
        let (repository, fs, base, tip, committer) = fixture();
        let mails = repository
            .format_patches(&[tip], &[base], &FormatPatchOptions::default())
            .unwrap()
            .into_iter()
            .map(crate::FormatPatch::into_data)
            .collect::<Vec<_>>();
        fs.write(Path::new("repo/second"), b"local\n").unwrap();
        assert!(
            repository
                .am(&mails, &committer, &AmOptions::default())
                .is_err()
        );
        assert_eq!(repository.am_state().unwrap().next, 1);
        assert_ne!(repository.resolve_reference("HEAD").unwrap(), base);
        repository
            .abort_am(&committer, &AmOptions::default())
            .unwrap();
        assert_eq!(repository.resolve_reference("HEAD").unwrap(), base);
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"one\n");
        assert_eq!(fs.read(Path::new("repo/second")).unwrap(), b"local\n");
    }

    #[test]
    fn skip_discards_failed_mail_and_applies_the_rest() {
        let (repository, fs, base, tip, committer) = fixture();
        let mut mails = repository
            .format_patches(&[tip], &[base], &FormatPatchOptions::default())
            .unwrap()
            .into_iter()
            .map(crate::FormatPatch::into_data)
            .collect::<Vec<_>>();
        let position = mails[0]
            .windows(5)
            .position(|value| value == b"-one\n")
            .unwrap();
        mails[0][position + 1..position + 4].copy_from_slice(b"bad");
        assert!(
            repository
                .am(&mails, &committer, &AmOptions::default())
                .is_err()
        );
        let progress = repository
            .skip_am(&committer, &AmOptions::default())
            .unwrap();
        assert_eq!(progress.commits.len(), 1);
        assert_eq!(fs.read(Path::new("repo/file")).unwrap(), b"one\n");
        assert_eq!(fs.read(Path::new("repo/second")).unwrap(), b"added\n");
    }

    #[test]
    fn decodes_folded_rfc2047_headers_and_dates() {
        let mail = b"From sender Mon Sep 17 00:00:00 2001\nFrom: =?UTF-8?Q?Ren=C3=A9e?= <renee@example.com>\nDate: Sat, 03 Feb 2001 04:05:06 -0000\nSubject: [PATCH] =?UTF-8?B?UsOpc3Vtw6k=?=\n\tdetails\n\n---\ndiff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
        let parsed = parse_mail(mail, &AmOptions::default()).unwrap();
        assert_eq!(parsed.author.name(), "Renée");
        assert!(parsed.author.has_unknown_timezone());
        assert_eq!(parsed.message, "Résumé details\n".as_bytes());
    }

    fn fixture() -> (Repository, MemoryFileSystem, ObjectId, ObjectId, Signature) {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let base = commit(&repository, &[], b"one\n", None, b"base\n", 100);
        let first = commit(&repository, &[base], b"two\n", None, b"change file\n", 200);
        let tip = commit(
            &repository,
            &[first],
            b"two\n",
            Some(b"added\n"),
            b"add second\n",
            300,
        );
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                base,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let tree = repository.read_commit(base, 1024 * 1024).unwrap().tree();
        repository
            .checkout_tree(
                tree,
                &CheckoutOptions {
                    force: true,
                    max_object_size: 1024 * 1024,
                },
            )
            .unwrap();
        let committer = Signature::new("Receiver", "receiver@example.com", 500, 0).unwrap();
        (repository, fs, base, tip, committer)
    }

    fn commit(
        repository: &Repository,
        parents: &[ObjectId],
        file: &[u8],
        second: Option<&[u8]>,
        message: &[u8],
        timestamp: i64,
    ) -> ObjectId {
        let file = repository.write_object(ObjectKind::Blob, file).unwrap();
        let mut entries = vec![TreeEntry::new(EntryMode::Blob, b"file".to_vec(), file).unwrap()];
        if let Some(second) = second {
            let second = repository.write_object(ObjectKind::Blob, second).unwrap();
            entries.push(TreeEntry::new(EntryMode::Blob, b"second".to_vec(), second).unwrap());
        }
        let tree = repository.write_tree(&Tree::new(entries).unwrap()).unwrap();
        let author = Signature::new("Mail Author", "mail@example.com", timestamp, 0).unwrap();
        let mut builder =
            CommitBuilder::new(tree, author.clone(), author).message(message.to_vec());
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }
}
