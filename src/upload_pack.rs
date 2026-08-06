//! Git protocol v0/v1 upload-pack advertisement and response generation.

use std::collections::{BTreeSet, HashSet};
use std::str::FromStr;

use crate::{
    Capability, EntryMode, Error, ObjectId, ObjectKind, PackOptions, PktLine, PktLineDecoder,
    ReferenceTarget, Repository, Result, Sideband,
};

const CAPABILITIES: &str =
    "side-band-64k ofs-delta no-progress object-format=sha1 agent=git-rs/0.1";

/// Limits and encoding choices for an upload-pack session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPackOptions {
    pub max_object_size: usize,
    pub max_objects: usize,
    pub max_tag_depth: usize,
    pub use_deltas: bool,
}

impl Default for UploadPackOptions {
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024 * 1024,
            max_objects: 10_000_000,
            max_tag_depth: 64,
            use_deltas: true,
        }
    }
}

/// A validated, complete v0/v1 client request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPackRequest {
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    capabilities: Vec<Capability>,
    done: bool,
}

impl UploadPackRequest {
    /// Parse a complete request containing a want section, flush, and
    /// negotiation section.
    ///
    /// # Errors
    /// Returns an error for malformed framing, invalid commands, capabilities
    /// outside the supported advertisement, or data following `done`.
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut decoder = PktLineDecoder::new();
        decoder.extend(input);
        let mut wants = Vec::new();
        let mut haves = Vec::new();
        let mut capabilities = Vec::new();
        let mut want_section = true;
        let mut done = false;
        while let Some(packet) = decoder.next_packet()? {
            match packet {
                PktLine::Flush if want_section => want_section = false,
                PktLine::Flush => {}
                PktLine::Data(mut line) => {
                    if done {
                        return protocol_error("data follows upload-pack `done`");
                    }
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    if want_section {
                        let (id, requested) = parse_want(&line, wants.is_empty())?;
                        if wants.contains(&id) {
                            return protocol_error(format!("duplicate want {id}"));
                        }
                        if wants.is_empty() {
                            validate_capabilities(&requested)?;
                            capabilities = requested;
                        } else if !requested.is_empty() {
                            return protocol_error("capabilities are only valid on the first want");
                        }
                        wants.push(id);
                    } else if let Some(value) = line.strip_prefix(b"have ") {
                        let id = parse_exact_id(value, "have")?;
                        if !haves.contains(&id) {
                            haves.push(id);
                        }
                    } else if line == b"done" {
                        done = true;
                    } else {
                        return protocol_error(format!(
                            "unexpected upload-pack command `{}`",
                            String::from_utf8_lossy(&line)
                        ));
                    }
                }
                PktLine::Delimiter | PktLine::ResponseEnd => {
                    return protocol_error("v2 control packet in v0/v1 upload-pack request");
                }
            }
        }
        decoder.finish()?;
        if want_section {
            return protocol_error("want section has no terminating flush");
        }
        if wants.is_empty() {
            return protocol_error("upload-pack request has no wants");
        }
        Ok(Self {
            wants,
            haves,
            capabilities,
            done,
        })
    }

    #[must_use]
    pub fn wants(&self) -> &[ObjectId] {
        &self.wants
    }

    #[must_use]
    pub fn haves(&self) -> &[ObjectId] {
        &self.haves
    }

    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    fn has_capability(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.name() == name)
    }
}

impl Repository {
    /// Advertise v0/v1 upload-pack refs and supported capabilities.
    ///
    /// # Errors
    /// Returns an error for malformed refs or storage failures.
    pub fn advertise_upload_pack(&self) -> Result<Vec<u8>> {
        let mut advertised = Vec::new();
        let head = self.read_reference("HEAD")?;
        let head_target = match head.target() {
            ReferenceTarget::Direct(id) => Some(*id),
            ReferenceTarget::Symbolic(_) => self.resolve_reference("HEAD").ok(),
        };
        if let Some(id) = head_target {
            advertised.push(("HEAD".to_owned(), id));
        }
        for reference in self.references()? {
            let id = match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(reference.name())?,
            };
            advertised.push((reference.name().to_owned(), id));
        }
        advertised.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if head_target.is_some() {
            advertised.sort_by_key(|(name, _)| name != "HEAD");
        }

        let mut output = Vec::new();
        if advertised.is_empty() {
            append_packet(
                &mut output,
                format!("{} capabilities^{{}}\0{CAPABILITIES}\n", ObjectId::null()).as_bytes(),
            )?;
        } else {
            for (index, (name, id)) in advertised.iter().enumerate() {
                let mut line = format!("{id} {name}").into_bytes();
                if index == 0 {
                    line.push(0);
                    line.extend_from_slice(CAPABILITIES.as_bytes());
                    if name == "HEAD"
                        && let ReferenceTarget::Symbolic(target) = head.target()
                    {
                        line.extend_from_slice(format!(" symref=HEAD:{target}").as_bytes());
                    }
                }
                line.push(b'\n');
                append_packet(&mut output, &line)?;
            }
        }
        output.extend(PktLine::Flush.encode()?);
        Ok(output)
    }

    /// Answer a v0/v1 upload-pack request with ACK/NAK and optional pack data.
    ///
    /// Without `side-band-64k`, raw pack bytes follow the negotiation pkt-line.
    /// With sideband enabled, pack bytes are emitted on channel 1 and followed
    /// by a flush packet.
    ///
    /// # Errors
    /// A negotiation round without `done` returns only ACK/NAK. Returns an
    /// error for a want outside the advertised refs, corrupt object graphs, or
    /// pack generation failures.
    pub fn respond_upload_pack(
        &self,
        request: &UploadPackRequest,
        options: &UploadPackOptions,
    ) -> Result<Vec<u8>> {
        let tips = self.advertised_tip_ids()?;
        if let Some(id) = request.wants.iter().find(|id| !tips.contains(id)) {
            return protocol_error(format!("want {id} is not an advertised ref"));
        }

        let common = self.reachable_objects_bounded(
            &request.haves,
            options.max_object_size,
            true,
            options.max_objects,
        )?;
        let acknowledged = request.haves.iter().rev().find(|id| common.contains(id));
        let mut response = Vec::new();
        let negotiation = acknowledged.map_or_else(
            || b"NAK\n".to_vec(),
            |id| format!("ACK {id}\n").into_bytes(),
        );
        append_packet(&mut response, &negotiation)?;
        if !request.done {
            return Ok(response);
        }

        let wanted = self.reachable_objects_bounded(
            &request.wants,
            options.max_object_size,
            false,
            options.max_objects,
        )?;
        let pack_ids = wanted
            .into_iter()
            .filter(|id| !common.contains(id))
            .collect::<Vec<_>>();
        let pack = self.build_pack(
            &pack_ids,
            &PackOptions {
                max_object_size: options.max_object_size,
                use_deltas: options.use_deltas && request.has_capability("ofs-delta"),
            },
        )?;
        if request.has_capability("side-band-64k") {
            for chunk in pack.pack().chunks(crate::protocol::MAX_PACKET_DATA_LEN - 1) {
                response.extend(Sideband::Data(chunk.to_vec()).encode()?);
            }
            response.extend(PktLine::Flush.encode()?);
        } else {
            response.extend_from_slice(pack.pack());
        }
        Ok(response)
    }

    fn advertised_tip_ids(&self) -> Result<HashSet<ObjectId>> {
        let mut ids = HashSet::new();
        if let Ok(id) = self.resolve_reference("HEAD") {
            ids.insert(id);
        }
        for reference in self.references()? {
            ids.insert(match reference.target() {
                ReferenceTarget::Direct(id) => *id,
                ReferenceTarget::Symbolic(_) => self.resolve_reference(reference.name())?,
            });
        }
        Ok(ids)
    }

    pub(crate) fn reachable_objects(
        &self,
        roots: &[ObjectId],
        max_size: usize,
        ignore_missing_roots: bool,
    ) -> Result<Vec<ObjectId>> {
        self.reachable_objects_bounded(roots, max_size, ignore_missing_roots, usize::MAX)
    }

    pub(crate) fn reachable_objects_bounded(
        &self,
        roots: &[ObjectId],
        max_size: usize,
        ignore_missing_roots: bool,
        max_objects: usize,
    ) -> Result<Vec<ObjectId>> {
        let mut seen = BTreeSet::new();
        let mut ordered = Vec::new();
        let mut stack = roots.iter().rev().copied().collect::<Vec<_>>();
        while let Some(id) = stack.pop() {
            if seen.contains(&id) {
                continue;
            }
            let object = match self.read_object(id, max_size) {
                Ok(object) => object,
                Err(Error::NotFound(_)) if ignore_missing_roots && roots.contains(&id) => continue,
                Err(error) => return Err(error),
            };
            seen.insert(id);
            if seen.len() > max_objects {
                return Err(Error::InvalidObject(
                    "reachable object traversal exceeds limit".into(),
                ));
            }
            ordered.push(id);
            match object.kind() {
                ObjectKind::Commit => {
                    let commit = crate::Commit::parse(object.data())?;
                    for parent in commit.parents().iter().rev() {
                        stack.push(*parent);
                    }
                    stack.push(commit.tree());
                }
                ObjectKind::Tree => {
                    let tree = crate::Tree::parse(object.data())?;
                    for entry in tree.entries().iter().rev() {
                        if entry.mode() != EntryMode::Gitlink {
                            stack.push(entry.id());
                        }
                    }
                }
                ObjectKind::Tag => stack.push(crate::AnnotatedTag::parse(object.data())?.target()),
                ObjectKind::Blob => {}
            }
        }
        Ok(ordered)
    }
}

fn parse_want(line: &[u8], first: bool) -> Result<(ObjectId, Vec<Capability>)> {
    let value = line
        .strip_prefix(b"want ")
        .ok_or_else(|| Error::Protocol("expected `want` command".into()))?;
    if value.len() < ObjectId::HEX_LENGTH {
        return protocol_error("truncated want object ID");
    }
    let id = parse_exact_id(&value[..ObjectId::HEX_LENGTH], "want")?;
    let remainder = &value[ObjectId::HEX_LENGTH..];
    if remainder.is_empty() {
        return Ok((id, Vec::new()));
    }
    if !first || remainder.first() != Some(&b' ') {
        return protocol_error("invalid want capability separator");
    }
    Ok((id, Capability::parse_list(&remainder[1..])?))
}

fn parse_exact_id(value: &[u8], command: &str) -> Result<ObjectId> {
    if value.len() != ObjectId::HEX_LENGTH || !value.is_ascii() {
        return protocol_error(format!("invalid {command} object ID"));
    }
    ObjectId::from_str(
        std::str::from_utf8(value).map_err(|_| Error::Protocol("object ID is not ASCII".into()))?,
    )
    .map_err(|_| Error::Protocol(format!("invalid {command} object ID")))
}

fn validate_capabilities(capabilities: &[Capability]) -> Result<()> {
    for capability in capabilities {
        let valid = match capability.name() {
            "side-band-64k" | "ofs-delta" | "no-progress" => capability.value().is_none(),
            "object-format" => capability.value() == Some("sha1"),
            "agent" => capability.value().is_some(),
            _ => false,
        };
        if !valid {
            return protocol_error(format!(
                "unsupported upload-pack capability `{}`",
                capability.name()
            ));
        }
    }
    Ok(())
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
    use super::{UploadPackOptions, UploadPackRequest};
    use crate::object::sha1;
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, PktLine,
        PktLineDecoder, PreviousValue, ReferenceName, Repository, Sideband, Signature, Tree,
        TreeEntry,
    };

    #[test]
    fn unborn_repository_advertises_capabilities_and_flushes() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let advertisement = repository.advertise_upload_pack().unwrap();
        let mut decoder = PktLineDecoder::new();
        decoder.extend(&advertisement);
        let Some(PktLine::Data(line)) = decoder.next_packet().unwrap() else {
            panic!("expected capability pseudo-ref");
        };
        assert!(line.starts_with(b"0000000000000000000000000000000000000000 capabilities^{}\0"));
        assert_eq!(decoder.next_packet().unwrap(), Some(PktLine::Flush));
        decoder.finish().unwrap();
    }

    #[test]
    fn parses_complete_requests_and_rejects_unadvertised_features() {
        let id = crate::ObjectId::compute(ObjectKind::Blob, b"wanted");
        let request = request_bytes(&[
            PktLine::Data(
                format!("want {id} side-band-64k ofs-delta object-format=sha1\n").into_bytes(),
            ),
            PktLine::Flush,
            PktLine::Data(b"done\n".to_vec()),
        ]);
        let parsed = UploadPackRequest::parse(&request).unwrap();
        assert_eq!(parsed.wants(), &[id]);
        assert!(parsed.is_done());

        let invalid = request_bytes(&[
            PktLine::Data(format!("want {id} thin-pack\n").into_bytes()),
            PktLine::Flush,
            PktLine::Data(b"done\n".to_vec()),
        ]);
        assert!(UploadPackRequest::parse(&invalid).is_err());
    }

    #[test]
    fn negotiates_common_history_and_sidebands_the_minimal_pack() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let identity = Signature::new("A", "a@example.com", 1, 0).unwrap();
        let base_blob = repository.write_object(ObjectKind::Blob, b"base").unwrap();
        let base_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), base_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let base_commit = repository
            .write_commit(
                &CommitBuilder::new(base_tree, identity.clone(), identity.clone())
                    .message(b"base\n".to_vec())
                    .build(),
            )
            .unwrap();
        let tip_blob = repository.write_object(ObjectKind::Blob, b"tip").unwrap();
        let tip_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), tip_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let tip_commit = repository
            .write_commit(
                &CommitBuilder::new(tip_tree, identity.clone(), identity)
                    .parent(base_commit)
                    .message(b"tip\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip_commit,
                PreviousValue::MustNotExist,
            )
            .unwrap();

        let request = request_bytes(&[
            PktLine::Data(
                format!("want {tip_commit} side-band-64k ofs-delta no-progress\n").into_bytes(),
            ),
            PktLine::Flush,
            PktLine::Data(format!("have {base_commit}\n").into_bytes()),
            PktLine::Data(b"done\n".to_vec()),
        ]);
        let request = UploadPackRequest::parse(&request).unwrap();
        let response = repository
            .respond_upload_pack(&request, &UploadPackOptions::default())
            .unwrap();

        let mut decoder = PktLineDecoder::new();
        decoder.extend(&response);
        assert_eq!(
            decoder.next_packet().unwrap(),
            Some(PktLine::Data(format!("ACK {base_commit}\n").into_bytes()))
        );
        let mut pack = Vec::new();
        loop {
            match decoder.next_packet().unwrap().unwrap() {
                PktLine::Data(data) => match Sideband::decode(&data).unwrap() {
                    Sideband::Data(data) => pack.extend(data),
                    Sideband::Progress(_) | Sideband::Error(_) => panic!("unexpected sideband"),
                },
                PktLine::Flush => break,
                PktLine::Delimiter | PktLine::ResponseEnd => panic!("unexpected control packet"),
            }
        }
        decoder.finish().unwrap();
        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(u32::from_be_bytes(pack[8..12].try_into().unwrap()), 3);
        assert_eq!(
            sha1::digest(&pack[..pack.len() - 20]),
            pack[pack.len() - 20..]
        );
    }

    #[test]
    fn rejects_wants_that_are_not_advertised_tips() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let hidden = repository
            .write_object(ObjectKind::Blob, b"hidden")
            .unwrap();
        let request = request_bytes(&[
            PktLine::Data(format!("want {hidden}\n").into_bytes()),
            PktLine::Flush,
            PktLine::Data(b"done\n".to_vec()),
        ]);
        let parsed = UploadPackRequest::parse(&request).unwrap();
        assert!(
            repository
                .respond_upload_pack(&parsed, &UploadPackOptions::default())
                .is_err()
        );
    }

    #[test]
    fn supports_stateless_negotiation_rounds_and_raw_pack_output() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let tip = repository.write_object(ObjectKind::Blob, b"tip").unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();

        let round = request_bytes(&[
            PktLine::Data(format!("want {tip}\n").into_bytes()),
            PktLine::Flush,
            PktLine::Data(format!("have {tip}\n").into_bytes()),
            PktLine::Flush,
        ]);
        let round = UploadPackRequest::parse(&round).unwrap();
        assert!(!round.is_done());
        assert_eq!(
            repository
                .respond_upload_pack(&round, &UploadPackOptions::default())
                .unwrap(),
            PktLine::Data(format!("ACK {tip}\n").into_bytes())
                .encode()
                .unwrap()
        );

        let final_request = request_bytes(&[
            PktLine::Data(format!("want {tip}\n").into_bytes()),
            PktLine::Flush,
            PktLine::Data(b"done\n".to_vec()),
        ]);
        let final_request = UploadPackRequest::parse(&final_request).unwrap();
        let response = repository
            .respond_upload_pack(&final_request, &UploadPackOptions::default())
            .unwrap();
        assert!(response.starts_with(b"0008NAK\nPACK"));
    }

    fn request_bytes(packets: &[PktLine]) -> Vec<u8> {
        packets
            .iter()
            .flat_map(|packet| packet.encode().unwrap())
            .collect()
    }
}
