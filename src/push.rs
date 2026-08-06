//! Transport-neutral receive-pack client orchestration.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Error, GraphOptions, ObjectId, PackOptions, PktLine, ReceivePackOptions, ReceivePackRequest,
    ReferenceName, RemoteAdvertisement, Repository, Result,
};

/// Byte exchange required from a receive-pack transport.
pub trait ReceivePackTransport {
    /// Obtain the protocol v0/v1 receive-pack advertisement.
    ///
    /// # Errors
    /// Returns transport, remote, or protocol errors.
    fn advertise(&mut self) -> Result<Vec<u8>>;

    /// Send one complete command-and-pack request and return its status report.
    ///
    /// # Errors
    /// Returns transport, remote, or protocol errors.
    fn request(&mut self, request: &[u8]) -> Result<Vec<u8>>;
}

/// A receive-pack transport connected directly to another repository.
pub struct InProcessReceivePackTransport<'a> {
    repository: &'a Repository,
    options: ReceivePackOptions,
}

impl<'a> InProcessReceivePackTransport<'a> {
    #[must_use]
    pub const fn new(repository: &'a Repository, options: ReceivePackOptions) -> Self {
        Self {
            repository,
            options,
        }
    }
}

impl ReceivePackTransport for InProcessReceivePackTransport<'_> {
    fn advertise(&mut self) -> Result<Vec<u8>> {
        self.repository.advertise_receive_pack()
    }

    fn request(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        let request = ReceivePackRequest::parse(request)?;
        Ok(self
            .repository
            .receive_pack(&request, &self.options)?
            .response)
    }
}

/// One destination ref change. `None` deletes the remote ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushUpdate {
    destination: ReferenceName,
    new_id: Option<ObjectId>,
    force: bool,
}

impl PushUpdate {
    #[must_use]
    pub const fn update(destination: ReferenceName, new_id: ObjectId) -> Self {
        Self {
            destination,
            new_id: Some(new_id),
            force: false,
        }
    }

    /// Construct an update which may replace a non-ancestor branch or an
    /// existing tag.
    #[must_use]
    pub const fn force_update(destination: ReferenceName, new_id: ObjectId) -> Self {
        Self {
            destination,
            new_id: Some(new_id),
            force: true,
        }
    }

    #[must_use]
    pub const fn delete(destination: ReferenceName) -> Self {
        Self {
            destination,
            new_id: None,
            force: false,
        }
    }

    #[must_use]
    pub const fn destination(&self) -> &ReferenceName {
        &self.destination
    }

    #[must_use]
    pub const fn new_id(&self) -> Option<ObjectId> {
        self.new_id
    }

    #[must_use]
    pub const fn is_force(&self) -> bool {
        self.force
    }
}

/// Push negotiation and pack-construction choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushOptions {
    pub atomic: bool,
    pub max_commits: usize,
    pub max_object_size: usize,
    pub use_deltas: bool,
}

impl Default for PushOptions {
    fn default() -> Self {
        Self {
            atomic: false,
            max_commits: 10_000_000,
            max_object_size: 1024 * 1024 * 1024,
            use_deltas: true,
        }
    }
}

/// Remote status for one requested destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushStatus {
    pub name: ReferenceName,
    pub error: Option<String>,
}

/// Parsed receive-pack result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushResult {
    pub statuses: Vec<PushStatus>,
    pub sent_objects: usize,
}

impl PushResult {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.statuses.iter().all(|status| status.error.is_none())
    }
}

impl Repository {
    /// Push explicit ref updates through an arbitrary receive-pack transport.
    ///
    /// The advertised old IDs are embedded in compare-and-swap commands. Only
    /// objects absent from the remote's advertised closure are packed.
    ///
    /// # Errors
    /// Returns an error for malformed advertisements or status reports,
    /// unsupported capabilities, missing local objects, pack construction, or
    /// transport failures. Remote per-ref rejection is returned in `PushResult`.
    pub fn push<T: ReceivePackTransport>(
        &self,
        transport: &mut T,
        updates: &[PushUpdate],
        options: &PushOptions,
    ) -> Result<PushResult> {
        if updates.is_empty() {
            return protocol_error("push requires at least one ref update");
        }
        let advertisement = RemoteAdvertisement::parse(&transport.advertise()?)?;
        validate_advertisement(&advertisement, updates, options)?;

        let remote = advertisement
            .refs()
            .iter()
            .filter(|reference| reference.name() != "capabilities^{}")
            .map(|reference| (reference.name(), reference.id()))
            .collect::<BTreeMap<_, _>>();
        let mut destinations = BTreeSet::new();
        for update in updates {
            if !destinations.insert(update.destination.clone()) {
                return protocol_error(format!(
                    "duplicate push destination `{}`",
                    update.destination
                ));
            }
        }
        self.validate_push_updates(&remote, updates, options)?;

        let new_roots = updates
            .iter()
            .filter_map(PushUpdate::new_id)
            .collect::<Vec<_>>();
        let remote_roots = remote.values().copied().collect::<Vec<_>>();
        let remote_objects = self
            .reachable_objects(&remote_roots, options.max_object_size, true)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let local_objects = self.reachable_objects(&new_roots, options.max_object_size, false)?;
        let pack_ids = local_objects
            .into_iter()
            .filter(|id| !remote_objects.contains(id))
            .collect::<Vec<_>>();
        let pack = self.build_pack(
            &pack_ids,
            &PackOptions {
                max_object_size: options.max_object_size,
                use_deltas: options.use_deltas && has_capability(&advertisement, "ofs-delta"),
            },
        )?;

        let request = build_request(&advertisement, &remote, updates, options, pack.pack())?;
        let response = transport.request(&request)?;
        let statuses = parse_status(&response, updates)?;
        Ok(PushResult {
            statuses,
            sent_objects: pack.object_count(),
        })
    }

    fn validate_push_updates(
        &self,
        remote: &BTreeMap<&str, ObjectId>,
        updates: &[PushUpdate],
        options: &PushOptions,
    ) -> Result<()> {
        for update in updates {
            let Some(new) = update.new_id else {
                continue;
            };
            let Some(old) = remote.get(update.destination.as_str()).copied() else {
                continue;
            };
            if old == new || update.force {
                continue;
            }
            if !update.destination.as_str().starts_with("refs/heads/")
                || !self.is_ancestor(
                    old,
                    new,
                    &GraphOptions {
                        max_commits: options.max_commits,
                        max_object_size: options.max_object_size,
                    },
                )?
            {
                return Err(Error::ReferenceConflict(format!(
                    "non-fast-forward update to {}",
                    update.destination
                )));
            }
        }
        Ok(())
    }
}

fn validate_advertisement(
    advertisement: &RemoteAdvertisement,
    updates: &[PushUpdate],
    options: &PushOptions,
) -> Result<()> {
    if !has_capability(advertisement, "report-status") {
        return protocol_error("remote does not support `report-status`");
    }
    if options.atomic && !has_capability(advertisement, "atomic") {
        return protocol_error("remote does not support `atomic`");
    }
    if updates.iter().any(|update| update.new_id.is_none())
        && !has_capability(advertisement, "delete-refs")
    {
        return protocol_error("remote does not support `delete-refs`");
    }
    if let Some(format) = advertisement
        .capabilities()
        .iter()
        .find(|capability| capability.name() == "object-format")
        .and_then(crate::Capability::value)
        && format != "sha1"
    {
        return protocol_error(format!("unsupported remote object format `{format}`"));
    }
    Ok(())
}

fn build_request(
    advertisement: &RemoteAdvertisement,
    remote: &BTreeMap<&str, ObjectId>,
    updates: &[PushUpdate],
    options: &PushOptions,
    pack: &[u8],
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for (index, update) in updates.iter().enumerate() {
        let old = remote
            .get(update.destination.as_str())
            .copied()
            .unwrap_or_else(ObjectId::null);
        let new = update.new_id.unwrap_or_else(ObjectId::null);
        let mut line = format!("{old} {new} {}", update.destination);
        if index == 0 {
            line.push('\0');
            line.push_str("report-status");
            if options.atomic {
                line.push_str(" atomic");
            }
            if options.use_deltas && has_capability(advertisement, "ofs-delta") {
                line.push_str(" ofs-delta");
            }
            if has_capability(advertisement, "object-format") {
                line.push_str(" object-format=sha1");
            }
            if has_capability(advertisement, "agent") {
                line.push_str(" agent=git-rs/0.1");
            }
        }
        line.push('\n');
        append_packet(&mut output, line.as_bytes())?;
    }
    output.extend(PktLine::Flush.encode()?);
    if updates.iter().any(|update| update.new_id.is_some()) {
        output.extend_from_slice(pack);
    }
    Ok(output)
}

fn parse_status(input: &[u8], updates: &[PushUpdate]) -> Result<Vec<PushStatus>> {
    let mut cursor = 0;
    let (packet, consumed) = PktLine::decode(input)?;
    cursor += consumed;
    let PktLine::Data(unpack) = packet else {
        return protocol_error("receive-pack status has no unpack line");
    };
    let unpack = unpack
        .strip_prefix(b"unpack ")
        .and_then(|line| line.strip_suffix(b"\n"))
        .ok_or_else(|| Error::Protocol("malformed unpack status".into()))?;
    if unpack != b"ok" {
        return protocol_error(format!(
            "remote unpack failed: {}",
            String::from_utf8_lossy(unpack)
        ));
    }

    let mut statuses = Vec::with_capacity(updates.len());
    for expected in updates {
        let (packet, consumed) = PktLine::decode(&input[cursor..])?;
        cursor += consumed;
        let PktLine::Data(line) = packet else {
            return protocol_error("missing receive command status");
        };
        let line = line
            .strip_suffix(b"\n")
            .ok_or_else(|| Error::Protocol("unterminated receive status".into()))?;
        let (name, error) = if let Some(name) = line.strip_prefix(b"ok ") {
            (name, None)
        } else if let Some(value) = line.strip_prefix(b"ng ") {
            let split = value
                .iter()
                .position(|byte| *byte == b' ')
                .ok_or_else(|| Error::Protocol("receive rejection has no reason".into()))?;
            (
                &value[..split],
                Some(String::from_utf8_lossy(&value[split + 1..]).into_owned()),
            )
        } else {
            return protocol_error("unknown receive command status");
        };
        if name != expected.destination.as_str().as_bytes() {
            return protocol_error("receive status does not match requested destination");
        }
        statuses.push(PushStatus {
            name: expected.destination.clone(),
            error,
        });
    }
    let (flush, consumed) = PktLine::decode(&input[cursor..])?;
    cursor += consumed;
    if flush != PktLine::Flush || cursor != input.len() {
        return protocol_error("trailing or malformed receive status data");
    }
    Ok(statuses)
}

fn has_capability(advertisement: &RemoteAdvertisement, name: &str) -> bool {
    advertisement
        .capabilities()
        .iter()
        .any(|capability| capability.name() == name)
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
    use super::{InProcessReceivePackTransport, PushOptions, PushUpdate, ReceivePackTransport};
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, PreviousValue,
        ReceivePackOptions, ReferenceName, Repository, Signature, Tree, TreeEntry,
    };

    #[test]
    fn creates_updates_and_deletes_a_remote_branch() {
        let local = repository("local", false);
        let remote = repository("remote.git", true);
        let main = ReferenceName::branch("main").unwrap();
        let base = commit(&local, None, b"base\n");
        local
            .update_reference(&main, base, PreviousValue::MustNotExist)
            .unwrap();

        let mut transport =
            InProcessReceivePackTransport::new(&remote, ReceivePackOptions::default());
        let created = local
            .push(
                &mut transport,
                &[PushUpdate::update(main.clone(), base)],
                &PushOptions::default(),
            )
            .unwrap();
        assert!(created.is_ok());
        assert_eq!(created.sent_objects, 3);
        assert_eq!(remote.resolve_reference("refs/heads/main").unwrap(), base);

        let tip = commit(&local, Some(base), b"tip\n");
        local
            .update_reference(&main, tip, PreviousValue::MustExist(base))
            .unwrap();
        let updated = local
            .push(
                &mut transport,
                &[PushUpdate::update(main.clone(), tip)],
                &PushOptions::default(),
            )
            .unwrap();
        assert!(updated.is_ok());
        assert_eq!(updated.sent_objects, 3);
        assert_eq!(remote.resolve_reference("refs/heads/main").unwrap(), tip);

        let rewritten = commit(&local, Some(base), b"rewritten\n");
        let rejected = local.push(
            &mut transport,
            &[PushUpdate::update(main.clone(), rewritten)],
            &PushOptions::default(),
        );
        assert!(matches!(rejected, Err(crate::Error::ReferenceConflict(_))));
        assert_eq!(remote.resolve_reference("refs/heads/main").unwrap(), tip);

        let deleted = local
            .push(
                &mut transport,
                &[PushUpdate::delete(main)],
                &PushOptions::default(),
            )
            .unwrap();
        assert!(deleted.is_ok());
        assert_eq!(deleted.sent_objects, 0);
        assert!(remote.resolve_reference("refs/heads/main").is_err());
    }

    #[test]
    fn atomic_push_reports_all_refs_when_one_changes_after_advertisement() {
        let local = repository("local", false);
        let remote = repository("remote.git", true);
        let main = ReferenceName::branch("main").unwrap();
        let topic = ReferenceName::branch("topic").unwrap();
        let base = commit(&local, None, b"base\n");
        let main_tip = commit(&local, Some(base), b"main\n");
        let topic_tip = commit(&local, Some(base), b"topic\n");
        for name in [&main, &topic] {
            remote.write_object(ObjectKind::Blob, b"unrelated").unwrap();
            remote
                .update_reference(name, base, PreviousValue::MustNotExist)
                .unwrap();
        }
        copy_closure(&local, &remote, base);
        let stale = commit(&remote, Some(base), b"concurrent\n");
        let mut transport = MutatingTransport {
            remote: &remote,
            mutate: Some((main.clone(), base, stale)),
        };

        let result = local
            .push(
                &mut transport,
                &[
                    PushUpdate::update(main.clone(), main_tip),
                    PushUpdate::update(topic.clone(), topic_tip),
                ],
                &PushOptions {
                    atomic: true,
                    ..PushOptions::default()
                },
            )
            .unwrap();
        assert!(!result.is_ok());
        assert!(result.statuses.iter().all(|status| status.error.is_some()));
        assert_eq!(remote.resolve_reference(topic.as_str()).unwrap(), base);
        assert_eq!(remote.resolve_reference(main.as_str()).unwrap(), stale);
    }

    struct MutatingTransport<'a> {
        remote: &'a Repository,
        mutate: Option<(ReferenceName, crate::ObjectId, crate::ObjectId)>,
    }

    impl ReceivePackTransport for MutatingTransport<'_> {
        fn advertise(&mut self) -> crate::Result<Vec<u8>> {
            self.remote.advertise_receive_pack()
        }

        fn request(&mut self, request: &[u8]) -> crate::Result<Vec<u8>> {
            if let Some((name, old, new)) = self.mutate.take() {
                self.remote
                    .update_reference(&name, new, PreviousValue::MustExist(old))?;
            }
            let parsed = crate::ReceivePackRequest::parse(request)?;
            Ok(self
                .remote
                .receive_pack(&parsed, &ReceivePackOptions::default())?
                .response)
        }
    }

    fn repository(path: &str, bare: bool) -> Repository {
        Repository::init(
            MemoryFileSystem::new(),
            path,
            &InitOptions {
                bare,
                ..InitOptions::default()
            },
        )
        .unwrap()
    }

    fn commit(
        repository: &Repository,
        parent: Option<crate::ObjectId>,
        data: &[u8],
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, data).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("Push", "push@example.com", 1, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature);
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository
            .write_commit(&builder.message(b"push\n".to_vec()).build())
            .unwrap()
    }

    fn copy_closure(source: &Repository, destination: &Repository, root: crate::ObjectId) {
        for id in source
            .reachable_objects(&[root], usize::MAX, false)
            .unwrap()
        {
            let object = source.read_object(id, usize::MAX).unwrap();
            assert_eq!(
                destination
                    .write_object(object.kind(), object.data())
                    .unwrap(),
                id
            );
        }
    }
}
