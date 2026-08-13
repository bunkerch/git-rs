//! Git protocol v0/v1 receive-pack advertisement and push processing.

use std::collections::{BTreeSet, HashSet};
use std::str::FromStr;

use crate::{
    Capability, EntryMode, Error, IncomingPackOptions, ObjectId, ObjectKind, PktLine,
    PreviousValue, ReferenceEdit, ReferenceName, ReferenceTarget, Repository, Result,
    ValidatedPack, WrittenPack,
};

const CAPABILITIES: &str =
    "report-status delete-refs atomic ofs-delta object-format=sha1 agent=git-rs/0.1";

/// Resource and repository-safety settings for receive-pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivePackOptions {
    pub max_pack_size: usize,
    pub max_object_size: usize,
    pub max_total_inflated_size: usize,
    pub use_deltas: bool,
    /// Refuse updates to the branch checked out by a non-bare repository.
    pub deny_current_branch: bool,
}

impl Default for ReceivePackOptions {
    fn default() -> Self {
        Self {
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
            use_deltas: true,
            deny_current_branch: true,
        }
    }
}

/// One requested compare-and-swap ref update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiveCommand {
    old: ObjectId,
    new: ObjectId,
    name: ReferenceName,
}

impl ReceiveCommand {
    #[must_use]
    pub const fn old_id(&self) -> ObjectId {
        self.old
    }

    #[must_use]
    pub const fn new_id(&self) -> ObjectId {
        self.new
    }

    #[must_use]
    pub const fn name(&self) -> &ReferenceName {
        &self.name
    }
}

/// A validated receive-pack request and its quarantined pack bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivePackRequest {
    commands: Vec<ReceiveCommand>,
    capabilities: Vec<Capability>,
    pack: Vec<u8>,
}

impl ReceivePackRequest {
    /// Parse command pkt-lines through their flush packet, preserving the raw
    /// pack stream which follows.
    ///
    /// # Errors
    /// Returns an error for malformed commands, duplicate refs, unsupported
    /// capabilities, or invalid pkt-line framing.
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut cursor = 0;
        let mut commands = Vec::new();
        let mut capabilities = Vec::new();
        let mut names = BTreeSet::new();
        loop {
            let (packet, consumed) = PktLine::decode(&input[cursor..])?;
            cursor = cursor
                .checked_add(consumed)
                .ok_or_else(|| Error::Protocol("receive-pack offset overflow".into()))?;
            match packet {
                PktLine::Flush => break,
                PktLine::Data(mut line) => {
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    let requested = if commands.is_empty() {
                        line.iter().position(|byte| *byte == 0).map(|nul| {
                            let values = line.split_off(nul + 1);
                            line.pop();
                            values
                        })
                    } else if line.contains(&0) {
                        return protocol_error("capabilities appear after first receive command");
                    } else {
                        None
                    };
                    let command = parse_command(&line)?;
                    if !names.insert(command.name.clone()) {
                        return protocol_error(format!(
                            "duplicate receive command for {}",
                            command.name
                        ));
                    }
                    if let Some(requested) = requested {
                        capabilities = Capability::parse_list(&requested)?;
                        validate_capabilities(&capabilities)?;
                    }
                    commands.push(command);
                }
                PktLine::Delimiter | PktLine::ResponseEnd => {
                    return protocol_error("v2 control packet in receive-pack request");
                }
            }
        }
        if commands.is_empty() {
            return protocol_error("receive-pack request contains no commands");
        }
        Ok(Self {
            commands,
            capabilities,
            pack: input[cursor..].to_vec(),
        })
    }

    #[must_use]
    pub fn commands(&self) -> &[ReceiveCommand] {
        &self.commands
    }

    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    #[must_use]
    pub fn pack(&self) -> &[u8] {
        &self.pack
    }

    fn has_capability(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.name() == name)
    }
}

/// Result of applying one receive command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiveCommandStatus {
    pub name: ReferenceName,
    pub error: Option<String>,
}

/// Receive-pack response bytes and publication details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivePackResult {
    pub response: Vec<u8>,
    pub statuses: Vec<ReceiveCommandStatus>,
    pub written_pack: Option<WrittenPack>,
}

impl Repository {
    /// Advertise refs and implemented receive-pack capabilities.
    ///
    /// # Errors
    /// Returns an error for malformed refs or storage failures.
    pub fn advertise_receive_pack(&self) -> Result<Vec<u8>> {
        let mut references = self.references()?;
        references.sort_unstable_by(|left, right| left.name().cmp(right.name()));
        let mut output = Vec::new();
        if references.is_empty() {
            append_packet(
                &mut output,
                format!("{} capabilities^{{}}\0{CAPABILITIES}\n", ObjectId::null()).as_bytes(),
            )?;
        } else {
            for (index, reference) in references.iter().enumerate() {
                let id = match reference.target() {
                    ReferenceTarget::Direct(id) => *id,
                    ReferenceTarget::Symbolic(_) => self.resolve_reference(reference.name())?,
                };
                let mut line = format!("{id} {}", reference.name()).into_bytes();
                if index == 0 {
                    line.push(0);
                    line.extend_from_slice(CAPABILITIES.as_bytes());
                }
                line.push(b'\n');
                append_packet(&mut output, &line)?;
            }
        }
        output.extend(PktLine::Flush.encode()?);
        Ok(output)
    }

    /// Validate, quarantine, publish, and apply a receive-pack request.
    ///
    /// Ref commands use compare-and-swap semantics. Commands which fail their
    /// old-ID check or connectivity check are reported independently and do
    /// not update their refs.
    ///
    /// Pack corruption is represented as an `unpack` status without mutation.
    ///
    /// # Errors
    /// Returns an error if status framing or repository storage operations fail.
    pub fn receive_pack(
        &self,
        request: &ReceivePackRequest,
        options: &ReceivePackOptions,
    ) -> Result<ReceivePackResult> {
        let validated = if request.pack.is_empty() {
            None
        } else {
            match self.validate_incoming_pack(
                &request.pack,
                &IncomingPackOptions {
                    max_pack_size: options.max_pack_size,
                    max_object_size: options.max_object_size,
                    max_total_inflated_size: options.max_total_inflated_size,
                    use_deltas: options.use_deltas && request.has_capability("ofs-delta"),
                },
            ) {
                Ok(pack) => Some(pack),
                Err(error) => {
                    let message = protocol_line_message(&error.to_string());
                    let statuses = request
                        .commands
                        .iter()
                        .map(|command| ReceiveCommandStatus {
                            name: command.name.clone(),
                            error: Some("unpacker error".to_owned()),
                        })
                        .collect::<Vec<_>>();
                    let response = if request.has_capability("report-status") {
                        report_status(&message, &statuses)?
                    } else {
                        Vec::new()
                    };
                    return Ok(ReceivePackResult {
                        response,
                        statuses,
                        written_pack: None,
                    });
                }
            }
        };
        let checked_out = self.checked_out_receive_ref(options)?;
        let ignore_case = if checked_out.is_some() {
            self.case_insensitive_refnames()?
        } else {
            false
        };

        let mut statuses = Vec::with_capacity(request.commands.len());
        let mut connected = HashSet::new();
        for command in &request.commands {
            let error = if checked_out.as_ref().is_some_and(|checked_out| {
                matches_checked_out(checked_out, &command.name, ignore_case)
            }) {
                Some("branch is currently checked out".to_owned())
            } else if !current_matches(self, command)? {
                Some("stale old object ID".to_owned())
            } else if !command.new.is_null()
                && let Err(error) = self.check_connectivity(
                    command.new,
                    validated.as_ref(),
                    options.max_object_size,
                    &mut connected,
                )
            {
                Some(format!("missing necessary objects: {error}"))
            } else {
                None
            };
            statuses.push(ReceiveCommandStatus {
                name: command.name.clone(),
                error,
            });
        }

        reject_atomic_group(request, &mut statuses);

        let written_pack = if statuses.iter().any(|status| status.error.is_none()) {
            validated
                .as_ref()
                .map(|pack| self.publish_validated_pack(pack))
                .transpose()?
        } else {
            None
        };
        self.apply_receive_commands(request, &mut statuses);

        let response = if request.has_capability("report-status") {
            report_status("ok", &statuses)?
        } else {
            Vec::new()
        };
        Ok(ReceivePackResult {
            response,
            statuses,
            written_pack,
        })
    }

    fn checked_out_receive_ref(
        &self,
        options: &ReceivePackOptions,
    ) -> Result<Option<ReferenceName>> {
        if !options.deny_current_branch || self.work_tree().is_none() {
            return Ok(None);
        }
        Ok(match self.read_reference("HEAD")?.target() {
            ReferenceTarget::Symbolic(name) => Some(name.clone()),
            ReferenceTarget::Direct(_) => None,
        })
    }

    fn apply_receive_commands(
        &self,
        request: &ReceivePackRequest,
        statuses: &mut [ReceiveCommandStatus],
    ) {
        if request.has_capability("atomic") {
            if statuses.iter().any(|status| status.error.is_some()) {
                return;
            }
            let edits = request
                .commands
                .iter()
                .map(command_edit)
                .collect::<Vec<_>>();
            if let Err(error) = self.apply_reference_transaction(&edits) {
                for status in statuses {
                    status.error = Some(format!("atomic transaction failed: {error}"));
                }
            }
            return;
        }
        for (command, status) in request.commands.iter().zip(statuses) {
            if status.error.is_some() {
                continue;
            }
            let edit = command_edit(command);
            let result = if let Some(new) = edit.new_id() {
                self.update_reference(edit.name(), new, edit.previous())
            } else {
                self.delete_reference(edit.name(), command.old)
            };
            if let Err(error) = result {
                status.error = Some(error.to_string());
            }
        }
    }

    fn check_connectivity(
        &self,
        root: ObjectId,
        incoming: Option<&ValidatedPack>,
        max_size: usize,
        connected: &mut HashSet<ObjectId>,
    ) -> Result<()> {
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: usize::MAX,
            max_object_size: max_size,
        })?;
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if connected.contains(&id) || !seen.insert(id) {
                continue;
            }
            let owned;
            let (kind, data) = if let Some(object) = incoming.and_then(|pack| pack.object(id)) {
                object
            } else {
                owned = self.read_object(id, max_size)?;
                (owned.kind(), owned.data())
            };
            match kind {
                ObjectKind::Commit => {
                    let (tree, parents) = crate::commit::parse_commit_links(data)?;
                    stack.push(tree);
                    if !shallow.contains(&id) {
                        stack.extend(parents);
                    }
                }
                ObjectKind::Tree => {
                    let tree = crate::Tree::parse(data)?;
                    stack.extend(
                        tree.entries()
                            .iter()
                            .filter(|entry| entry.mode() != EntryMode::Gitlink)
                            .map(crate::TreeEntry::id),
                    );
                }
                ObjectKind::Tag => stack.push(crate::AnnotatedTag::parse(data)?.target()),
                ObjectKind::Blob => {}
            }
        }
        connected.extend(seen);
        Ok(())
    }
}

fn current_matches(repository: &Repository, command: &ReceiveCommand) -> Result<bool> {
    match repository.resolve_reference(command.name.as_str()) {
        Ok(current) => Ok(!command.old.is_null() && current == command.old),
        Err(Error::NotFound(_)) => Ok(command.old.is_null()),
        Err(error) => Err(error),
    }
}

fn matches_checked_out(
    checked_out: &ReferenceName,
    name: &ReferenceName,
    ignore_case: bool,
) -> bool {
    if ignore_case {
        checked_out.as_str().eq_ignore_ascii_case(name.as_str())
    } else {
        checked_out.as_str() == name.as_str()
    }
}

fn reject_atomic_group(request: &ReceivePackRequest, statuses: &mut [ReceiveCommandStatus]) {
    if request.has_capability("atomic") && statuses.iter().any(|status| status.error.is_some()) {
        for status in statuses {
            if status.error.is_none() {
                status.error = Some("atomic push failure".to_owned());
            }
        }
    }
}

fn command_edit(command: &ReceiveCommand) -> ReferenceEdit {
    if command.new.is_null() {
        ReferenceEdit::delete(command.name.clone(), command.old)
    } else {
        let previous = if command.old.is_null() {
            PreviousValue::MustNotExist
        } else {
            PreviousValue::MustExist(command.old)
        };
        ReferenceEdit::update(command.name.clone(), command.new, previous)
    }
}

fn parse_command(line: &[u8]) -> Result<ReceiveCommand> {
    if line.len() < 82 || line[40] != b' ' || line[81] != b' ' {
        return protocol_error("expected `<old> <new> <ref>` receive command");
    }
    let old = parse_id(&line[..40])?;
    let new = parse_id(&line[41..81])?;
    if old.is_null() && new.is_null() {
        return protocol_error("receive command has two null object IDs");
    }
    let name = std::str::from_utf8(&line[82..])
        .map_err(|_| Error::Protocol("receive refname is not UTF-8".into()))?;
    Ok(ReceiveCommand {
        old,
        new,
        name: ReferenceName::new(name.to_owned())?,
    })
}

fn parse_id(data: &[u8]) -> Result<ObjectId> {
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::Protocol("receive object ID is not ASCII".into()))?;
    ObjectId::from_str(text).map_err(|_| Error::Protocol("invalid receive object ID".into()))
}

fn validate_capabilities(capabilities: &[Capability]) -> Result<()> {
    for capability in capabilities {
        let valid = match capability.name() {
            "report-status" | "delete-refs" | "atomic" | "ofs-delta" => {
                capability.value().is_none()
            }
            "object-format" => capability.value() == Some("sha1"),
            "agent" => capability.value().is_some(),
            _ => false,
        };
        if !valid {
            return protocol_error(format!(
                "unsupported receive-pack capability `{}`",
                capability.name()
            ));
        }
    }
    Ok(())
}

fn report_status(unpack: &str, statuses: &[ReceiveCommandStatus]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    append_packet(&mut output, format!("unpack {unpack}\n").as_bytes())?;
    for status in statuses {
        let line = status.error.as_ref().map_or_else(
            || format!("ok {}\n", status.name),
            |error| format!("ng {} {error}\n", status.name),
        );
        append_packet(&mut output, line.as_bytes())?;
    }
    output.extend(PktLine::Flush.encode()?);
    Ok(output)
}

fn protocol_line_message(message: &str) -> String {
    message
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' || character == '\0' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn append_packet(output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    output.extend(PktLine::Data(data.to_vec()).encode()?);
    Ok(())
}

fn protocol_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{ReceivePackOptions, ReceivePackRequest};
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectId, ObjectKind, PackOptions,
        PktLine, PktLineDecoder, Repository, Signature, Tree, TreeEntry,
    };

    #[test]
    fn empty_repository_advertises_receive_capabilities() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let bytes = repository.advertise_receive_pack().unwrap();
        let (packet, consumed) = PktLine::decode(&bytes).unwrap();
        let PktLine::Data(line) = packet else {
            panic!("expected capability pseudo-ref");
        };
        assert!(
            line.windows(b"report-status".len())
                .any(|part| part == b"report-status")
        );
        assert_eq!(
            PktLine::decode(&bytes[consumed..]).unwrap().0,
            PktLine::Flush
        );
    }

    #[test]
    fn receives_quarantined_objects_updates_ref_and_reports_status() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let blob = source.write_object(ObjectKind::Blob, b"pushed").unwrap();
        let tree = source
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("Pusher", "push@example.com", 1, 0).unwrap();
        let commit = source
            .write_commit(
                &CommitBuilder::new(tree, identity.clone(), identity)
                    .message(b"push\n".to_vec())
                    .build(),
            )
            .unwrap();
        let pack = source
            .build_pack(&[commit, tree, blob], &PackOptions::default())
            .unwrap();

        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let input = receive_input(
            ObjectId::null(),
            commit,
            "refs/heads/main",
            "report-status ofs-delta object-format=sha1",
            pack.pack(),
        );
        let request = ReceivePackRequest::parse(&input).unwrap();
        assert_eq!(request.pack(), pack.pack());
        let result = destination
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(
            destination.resolve_reference("refs/heads/main").unwrap(),
            commit
        );
        assert_eq!(
            destination.read_object(blob, 1024).unwrap().data(),
            b"pushed"
        );
        assert!(result.written_pack.is_some());
        assert_eq!(result.statuses[0].error, None);

        let mut decoder = PktLineDecoder::new();
        decoder.extend(&result.response);
        assert_eq!(
            decoder.next_packet().unwrap(),
            Some(PktLine::Data(b"unpack ok\n".to_vec()))
        );
        assert_eq!(
            decoder.next_packet().unwrap(),
            Some(PktLine::Data(b"ok refs/heads/main\n".to_vec()))
        );
        assert_eq!(decoder.next_packet().unwrap(), Some(PktLine::Flush));
    }

    #[test]
    fn stale_creation_is_rejected_without_publishing_pack() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(
            fs.clone(),
            "repo",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        repository.create_branch("main", current, false).unwrap();
        let replacement = repository
            .write_object(ObjectKind::Blob, b"replacement")
            .unwrap();
        let source = repository
            .build_pack(&[replacement], &PackOptions::default())
            .unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            ObjectId::null(),
            replacement,
            "refs/heads/main",
            "report-status",
            source.pack(),
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert!(
            result.statuses[0]
                .error
                .as_deref()
                .unwrap()
                .contains("stale")
        );
        assert!(result.written_pack.is_none());
        assert_eq!(
            repository.resolve_reference("refs/heads/main").unwrap(),
            current
        );
    }

    #[test]
    fn corrupt_pack_reports_unpack_failure_without_mutation() {
        let source =
            Repository::init(MemoryFileSystem::new(), "source", &InitOptions::default()).unwrap();
        let object = source.write_object(ObjectKind::Blob, b"object").unwrap();
        let mut pack = source
            .build_pack(&[object], &PackOptions::default())
            .unwrap()
            .pack()
            .to_vec();
        let last = pack.len() - 1;
        pack[last] ^= 1;
        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            ObjectId::null(),
            object,
            "refs/heads/main",
            "report-status",
            &pack,
        ))
        .unwrap();
        let result = destination
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert!(result.written_pack.is_none());
        assert_eq!(result.statuses[0].error.as_deref(), Some("unpacker error"));
        let (first, _) = PktLine::decode(&result.response).unwrap();
        let PktLine::Data(line) = first else {
            panic!("expected unpack status");
        };
        assert!(line.starts_with(b"unpack "));
        assert_ne!(line, b"unpack ok\n");
        assert!(destination.resolve_reference("refs/heads/main").is_err());
    }

    #[test]
    fn accepts_deletion_and_rejects_duplicate_command_names_during_parsing() {
        let old = ObjectId::compute(ObjectKind::Blob, b"old");
        let deletion = receive_input(old, ObjectId::null(), "refs/heads/main", "", &[]);
        assert!(ReceivePackRequest::parse(&deletion).is_ok());

        let new = ObjectId::compute(ObjectKind::Blob, b"new");
        let first = format!(
            "{} {new} refs/heads/main\0report-status\n",
            ObjectId::null()
        );
        let second = format!("{} {new} refs/heads/main\n", ObjectId::null());
        let mut duplicate = PktLine::Data(first.into_bytes()).encode().unwrap();
        duplicate.extend(PktLine::Data(second.into_bytes()).encode().unwrap());
        duplicate.extend(PktLine::Flush.encode().unwrap());
        assert!(ReceivePackRequest::parse(&duplicate).is_err());
    }

    #[test]
    fn deletes_a_ref_when_delete_refs_was_advertised() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let old = repository.write_object(ObjectKind::Blob, b"old").unwrap();
        repository.create_branch("obsolete", old, false).unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            old,
            ObjectId::null(),
            "refs/heads/obsolete",
            "report-status",
            &[],
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(result.statuses[0].error, None);
        assert!(result.written_pack.is_none());
        assert!(repository.resolve_reference("refs/heads/obsolete").is_err());
        let (_, consumed) = PktLine::decode(&result.response).unwrap();
        assert_eq!(
            PktLine::decode(&result.response[consumed..]).unwrap().0,
            PktLine::Data(b"ok refs/heads/obsolete\n".to_vec())
        );
    }

    #[test]
    fn atomic_push_rejects_every_command_when_one_is_stale() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        let stale = ObjectId::compute(ObjectKind::Blob, b"stale");
        repository.create_branch("main", current, false).unwrap();
        let first = format!(
            "{} {current} refs/heads/topic\0report-status atomic\n",
            ObjectId::null()
        );
        let second = format!("{stale} {current} refs/heads/main\n");
        let mut input = PktLine::Data(first.into_bytes()).encode().unwrap();
        input.extend(PktLine::Data(second.into_bytes()).encode().unwrap());
        input.extend(PktLine::Flush.encode().unwrap());
        let request = ReceivePackRequest::parse(&input).unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert!(
            result.statuses[0]
                .error
                .as_deref()
                .unwrap()
                .contains("atomic")
        );
        assert!(
            result.statuses[1]
                .error
                .as_deref()
                .unwrap()
                .contains("stale")
        );
        assert!(repository.resolve_reference("refs/heads/topic").is_err());
        assert_eq!(
            repository.resolve_reference("refs/heads/main").unwrap(),
            current
        );
    }

    #[test]
    fn checked_out_branch_update_is_rejected_by_exact_name() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        repository.create_branch("main", current, false).unwrap();
        let replacement = repository
            .write_object(ObjectKind::Blob, b"replacement")
            .unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            current,
            replacement,
            "refs/heads/main",
            "report-status",
            &[],
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(
            result.statuses[0].error.as_deref(),
            Some("branch is currently checked out")
        );
        assert_eq!(
            repository.resolve_reference("refs/heads/main").unwrap(),
            current
        );
    }

    #[test]
    fn case_only_variant_update_succeeds_without_ignorecase() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        repository.create_branch("main", current, false).unwrap();
        let replacement = repository
            .write_object(ObjectKind::Blob, b"replacement")
            .unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            ObjectId::null(),
            replacement,
            "refs/heads/Main",
            "report-status",
            &[],
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(result.statuses[0].error, None);
        assert_eq!(
            repository.resolve_reference("refs/heads/Main").unwrap(),
            replacement
        );
        assert_eq!(
            repository.resolve_reference("refs/heads/main").unwrap(),
            current
        );
    }

    #[test]
    fn case_only_variant_update_is_rejected_with_ignorecase() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        repository.create_branch("main", current, false).unwrap();
        enable_ignorecase(&repository);
        let replacement = repository
            .write_object(ObjectKind::Blob, b"replacement")
            .unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            ObjectId::null(),
            replacement,
            "refs/heads/Main",
            "report-status",
            &[],
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(
            result.statuses[0].error.as_deref(),
            Some("branch is currently checked out")
        );
        assert_eq!(
            repository.resolve_reference("refs/heads/main").unwrap(),
            current
        );
        assert!(repository.resolve_reference("refs/heads/Main").is_err());
    }

    #[test]
    fn non_checked_out_branch_update_still_succeeds() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let current = repository
            .write_object(ObjectKind::Blob, b"current")
            .unwrap();
        repository.create_branch("main", current, false).unwrap();
        let topic = repository.write_object(ObjectKind::Blob, b"topic").unwrap();
        let request = ReceivePackRequest::parse(&receive_input(
            ObjectId::null(),
            topic,
            "refs/heads/topic",
            "report-status",
            &[],
        ))
        .unwrap();
        let result = repository
            .receive_pack(&request, &ReceivePackOptions::default())
            .unwrap();
        assert_eq!(result.statuses[0].error, None);
        assert_eq!(
            repository.resolve_reference("refs/heads/topic").unwrap(),
            topic
        );
    }

    fn enable_ignorecase(repository: &Repository) {
        let mut config = repository.read_config().unwrap();
        config.set("core.ignorecase", b"true").unwrap();
        repository.write_config(&config).unwrap();
    }

    fn receive_input(
        old: ObjectId,
        new: ObjectId,
        name: &str,
        capabilities: &str,
        pack: &[u8],
    ) -> Vec<u8> {
        let mut line = format!("{old} {new} {name}").into_bytes();
        line.push(0);
        line.extend_from_slice(capabilities.as_bytes());
        line.push(b'\n');
        let mut input = PktLine::Data(line).encode().unwrap();
        input.extend(PktLine::Flush.encode().unwrap());
        input.extend_from_slice(pack);
        input
    }
}
