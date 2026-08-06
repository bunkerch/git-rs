//! Git wire protocol v2 upload-pack capability, `ls-refs`, and `fetch` commands.

use std::collections::BTreeSet;
use std::str::FromStr;

use crate::{
    Error, ObjectId, ObjectKind, PackOptions, PktLine, PktLineDecoder, ReferenceTarget, Repository,
    Result, Sideband, UploadPackOptions,
};

/// Parser and response resource bounds for one protocol-v2 command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPackV2Limits {
    pub max_request_bytes: usize,
    pub max_capabilities: usize,
    pub max_arguments: usize,
    pub max_prefixes: usize,
    pub max_refs: usize,
    pub max_tag_depth: usize,
}

impl Default for UploadPackV2Limits {
    fn default() -> Self {
        Self {
            max_request_bytes: 64 * 1024 * 1024,
            max_capabilities: 1024,
            max_arguments: 1_000_000,
            max_prefixes: 1_000_000,
            max_refs: 1_000_000,
            max_tag_depth: 64,
        }
    }
}

/// A validated `ls-refs` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsRefsRequest {
    symrefs: bool,
    peel: bool,
    unborn: bool,
    prefixes: Vec<Vec<u8>>,
}

impl LsRefsRequest {
    #[must_use]
    pub const fn symrefs(&self) -> bool {
        self.symrefs
    }

    #[must_use]
    pub const fn peel(&self) -> bool {
        self.peel
    }

    #[must_use]
    pub const fn unborn(&self) -> bool {
        self.unborn
    }

    #[must_use]
    pub fn prefixes(&self) -> &[Vec<u8>] {
        &self.prefixes
    }
}

/// A validated base protocol-v2 `fetch` request.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FetchV2Request {
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    done: bool,
    ofs_delta: bool,
    include_tag: bool,
    shallow: Vec<ObjectId>,
    depth: Option<usize>,
    deepen_relative: bool,
}

impl FetchV2Request {
    #[must_use]
    pub fn wants(&self) -> &[ObjectId] {
        &self.wants
    }

    #[must_use]
    pub fn haves(&self) -> &[ObjectId] {
        &self.haves
    }

    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }
}

/// One complete protocol-v2 command request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadPackV2Request {
    Empty,
    LsRefs(LsRefsRequest),
    Fetch(FetchV2Request),
}

impl UploadPackV2Request {
    /// Parse one complete command request including its terminating flush.
    ///
    /// # Errors
    /// Returns an error for malformed framing, unsupported capabilities or
    /// arguments, duplicates, invalid object IDs, or exceeded limits.
    pub fn parse(input: &[u8], limits: &UploadPackV2Limits) -> Result<Self> {
        if input.len() > limits.max_request_bytes {
            return protocol_error("protocol v2 request exceeds byte limit");
        }
        let packets = decode_request(input)?;
        if packets.len() == 1 && packets[0] == PktLine::Flush {
            return Ok(Self::Empty);
        }
        let (command, arguments) =
            parse_command(&packets, limits.max_capabilities, limits.max_arguments)?;
        match command.as_slice() {
            b"ls-refs" => parse_ls_refs(arguments, limits).map(Self::LsRefs),
            b"fetch" => parse_fetch(arguments).map(Self::Fetch),
            _ => protocol_error(format!(
                "unsupported protocol v2 command `{}`",
                String::from_utf8_lossy(&command)
            )),
        }
    }
}

impl Repository {
    /// Advertise the implemented protocol-v2 upload-pack commands.
    ///
    /// # Errors
    /// Returns an error only if a capability cannot be pkt-line encoded.
    pub fn advertise_upload_pack_v2(&self) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for line in [
            b"version 2\n".as_slice(),
            b"agent=git-rs/0.1\n",
            b"ls-refs=unborn\n",
            b"fetch=shallow\n",
            b"object-format=sha1\n",
        ] {
            append_packet(&mut output, line)?;
        }
        output.extend(PktLine::Flush.encode()?);
        Ok(output)
    }

    /// Respond to one parsed protocol-v2 command.
    ///
    /// # Errors
    /// Returns an error for corrupt refs/objects, unavailable wants, exceeded
    /// limits, or pack construction failures.
    pub fn respond_upload_pack_v2(
        &self,
        request: &UploadPackV2Request,
        options: &UploadPackOptions,
        limits: &UploadPackV2Limits,
    ) -> Result<Vec<u8>> {
        match request {
            UploadPackV2Request::Empty => Ok(PktLine::Flush.encode()?),
            UploadPackV2Request::LsRefs(request) => self.respond_ls_refs(request, options, limits),
            UploadPackV2Request::Fetch(request) => self.respond_fetch_v2(request, options),
        }
    }

    fn respond_ls_refs(
        &self,
        request: &LsRefsRequest,
        options: &UploadPackOptions,
        limits: &UploadPackV2Limits,
    ) -> Result<Vec<u8>> {
        let head = self.read_reference("HEAD")?;
        let mut refs = self.references()?;
        refs.sort_unstable_by(|left, right| left.name().cmp(right.name()));
        let mut output = Vec::new();
        let head_selected = selected(b"HEAD", &request.prefixes);
        if head_selected {
            match head.target() {
                ReferenceTarget::Direct(id) => {
                    append_v2_ref(&mut output, *id, b"HEAD", None, None)?;
                }
                ReferenceTarget::Symbolic(target) => match self.resolve_reference("HEAD") {
                    Ok(id) => append_v2_ref(
                        &mut output,
                        id,
                        b"HEAD",
                        request.symrefs.then_some(target.as_str().as_bytes()),
                        None,
                    )?,
                    Err(Error::NotFound(_)) if request.unborn => append_unborn(
                        &mut output,
                        b"HEAD",
                        request.symrefs.then_some(target.as_str().as_bytes()),
                    )?,
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                },
            }
        }
        let mut emitted = usize::from(head_selected && !output.is_empty());
        if emitted > limits.max_refs {
            return protocol_error("ls-refs exceeds reference limit");
        }
        for reference in refs {
            if !selected(reference.name().as_bytes(), &request.prefixes) {
                continue;
            }
            emitted = emitted.saturating_add(1);
            if emitted > limits.max_refs {
                return protocol_error("ls-refs exceeds reference limit");
            }
            let (id, symref) = match reference.target() {
                ReferenceTarget::Direct(id) => (*id, None),
                ReferenceTarget::Symbolic(target) => (
                    self.resolve_reference(reference.name())?,
                    request.symrefs.then_some(target.as_str().as_bytes()),
                ),
            };
            let peeled = if request.peel && reference.name().starts_with("refs/tags/") {
                let peeled = self.peel_tag(id, limits.max_tag_depth, options.max_object_size)?;
                (peeled.id != id).then_some(peeled.id)
            } else {
                None
            };
            append_v2_ref(&mut output, id, reference.name().as_bytes(), symref, peeled)?;
        }
        output.extend(PktLine::Flush.encode()?);
        Ok(output)
    }

    #[allow(clippy::too_many_lines)]
    fn respond_fetch_v2(
        &self,
        request: &FetchV2Request,
        options: &UploadPackOptions,
    ) -> Result<Vec<u8>> {
        if request.wants.is_empty() {
            return protocol_error("fetch request has no wants");
        }
        for want in &request.wants {
            self.read_object(*want, options.max_object_size)?;
        }
        let mut shallow = request.shallow.iter().copied().collect::<BTreeSet<_>>();
        shallow.extend(self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_objects,
            max_object_size: options.max_object_size,
        })?);
        let common = self.reachable_objects_stopping_at(
            &request.haves,
            options.max_object_size,
            true,
            options.max_objects,
            &shallow,
        )?;
        if !request.done {
            let mut response = Vec::new();
            append_packet(&mut response, b"acknowledgments\n")?;
            let acknowledged = request
                .haves
                .iter()
                .filter(|id| common.contains(id))
                .collect::<Vec<_>>();
            if acknowledged.is_empty() {
                append_packet(&mut response, b"NAK\n")?;
            } else {
                for id in acknowledged {
                    append_packet(&mut response, format!("ACK {id}\n").as_bytes())?;
                }
            }
            response.extend(PktLine::Flush.encode()?);
            return Ok(response);
        }
        let (mut wanted, boundaries, unshallow) = if let Some(depth) = request.depth {
            if request.deepen_relative {
                self.reachable_objects_deepen_relative(
                    &request.wants,
                    &request.shallow.iter().copied().collect(),
                    depth,
                    options.max_object_size,
                    options.max_objects,
                )?
            } else {
                let (wanted, boundaries) = self.reachable_objects_at_depth(
                    &request.wants,
                    depth,
                    options.max_object_size,
                    options.max_objects,
                )?;
                let unshallow = request
                    .shallow
                    .iter()
                    .copied()
                    .filter(|id| !boundaries.contains(id) && wanted.contains(id))
                    .collect();
                (wanted, boundaries, unshallow)
            }
        } else {
            (
                self.reachable_objects_bounded(
                    &request.wants,
                    options.max_object_size,
                    false,
                    options.max_objects,
                )?,
                BTreeSet::new(),
                BTreeSet::new(),
            )
        };
        if request.include_tag {
            self.include_reachable_tags(
                &mut wanted,
                options.max_object_size,
                options.max_objects,
                options.max_tag_depth,
            )?;
        }
        let pack_ids = wanted
            .iter()
            .copied()
            .filter(|id| !common.contains(id))
            .collect::<Vec<_>>();
        let pack = self.build_pack(
            &pack_ids,
            &PackOptions {
                max_object_size: options.max_object_size,
                use_deltas: options.use_deltas && request.ofs_delta,
            },
        )?;
        let mut response = Vec::new();
        if request.depth.is_some() {
            append_packet(&mut response, b"shallow-info\n")?;
            for id in &boundaries {
                append_packet(&mut response, format!("shallow {id}\n").as_bytes())?;
            }
            for id in &unshallow {
                append_packet(&mut response, format!("unshallow {id}\n").as_bytes())?;
            }
            response.extend(PktLine::Delimiter.encode()?);
        }
        append_packet(&mut response, b"packfile\n")?;
        for chunk in pack.pack().chunks(crate::protocol::MAX_PACKET_DATA_LEN - 1) {
            response.extend(Sideband::Data(chunk.to_vec()).encode()?);
        }
        response.extend(PktLine::Flush.encode()?);
        Ok(response)
    }

    fn include_reachable_tags(
        &self,
        wanted: &mut Vec<ObjectId>,
        max_size: usize,
        max_objects: usize,
        max_tag_depth: usize,
    ) -> Result<()> {
        let reachable = wanted.iter().copied().collect::<BTreeSet<_>>();
        for reference in self.references()? {
            if !reference.name().starts_with("refs/tags/") {
                continue;
            }
            let ReferenceTarget::Direct(tag_id) = reference.target() else {
                continue;
            };
            if self.read_object(*tag_id, max_size)?.kind() == ObjectKind::Tag {
                let peeled = self.peel_tag(*tag_id, max_tag_depth, max_size)?;
                if reachable.contains(&peeled.id) {
                    for id in
                        self.reachable_objects_bounded(&[*tag_id], max_size, false, max_objects)?
                    {
                        if !wanted.contains(&id) {
                            wanted.push(id);
                            if wanted.len() > max_objects {
                                return protocol_error("fetch object count exceeds limit");
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn decode_request(input: &[u8]) -> Result<Vec<PktLine>> {
    let mut decoder = PktLineDecoder::new();
    decoder.extend(input);
    let mut packets = Vec::new();
    while let Some(packet) = decoder.next_packet()? {
        packets.push(packet);
    }
    decoder.finish()?;
    Ok(packets)
}

fn parse_command(
    packets: &[PktLine],
    max_capabilities: usize,
    max_arguments: usize,
) -> Result<(Vec<u8>, &[PktLine])> {
    let Some(PktLine::Data(first)) = packets.first() else {
        return protocol_error("protocol v2 request has no command");
    };
    let command = line(first)
        .strip_prefix(b"command=")
        .ok_or_else(|| Error::Protocol("protocol v2 request does not start with command".into()))?
        .to_vec();
    let delimiter = packets
        .iter()
        .position(|packet| *packet == PktLine::Delimiter)
        .ok_or_else(|| Error::Protocol("protocol v2 request has no delimiter".into()))?;
    if delimiter.saturating_sub(1) > max_capabilities {
        return protocol_error("protocol v2 capability count exceeds limit");
    }
    validate_request_capabilities(&packets[1..delimiter])?;
    if packets.last() != Some(&PktLine::Flush) {
        return protocol_error("protocol v2 request has no terminating flush");
    }
    let arguments = &packets[delimiter + 1..packets.len() - 1];
    if arguments.len() > max_arguments {
        return protocol_error("protocol v2 argument count exceeds limit");
    }
    Ok((command, arguments))
}

fn validate_request_capabilities(packets: &[PktLine]) -> Result<()> {
    for packet in packets {
        let PktLine::Data(value) = packet else {
            return protocol_error("control packet in protocol v2 capability section");
        };
        let value = line(value);
        let valid = value == b"object-format=sha1" || value.starts_with(b"agent=");
        if !valid {
            return protocol_error(format!(
                "unsupported protocol v2 capability `{}`",
                String::from_utf8_lossy(value)
            ));
        }
    }
    Ok(())
}

fn parse_ls_refs(arguments: &[PktLine], limits: &UploadPackV2Limits) -> Result<LsRefsRequest> {
    let mut request = LsRefsRequest {
        symrefs: false,
        peel: false,
        unborn: false,
        prefixes: Vec::new(),
    };
    for argument in data_lines(arguments)? {
        match argument {
            b"symrefs" => set_once(&mut request.symrefs, "symrefs")?,
            b"peel" => set_once(&mut request.peel, "peel")?,
            b"unborn" => set_once(&mut request.unborn, "unborn")?,
            value if value.starts_with(b"ref-prefix ") => {
                let prefix = &value[b"ref-prefix ".len()..];
                if prefix.contains(&0) || prefix.is_empty() {
                    return protocol_error("invalid ls-refs prefix");
                }
                if !request.prefixes.contains(&prefix.to_vec()) {
                    request.prefixes.push(prefix.to_vec());
                }
                if request.prefixes.len() > limits.max_prefixes {
                    return protocol_error("ls-refs prefix count exceeds limit");
                }
            }
            value => {
                return protocol_error(format!(
                    "unsupported ls-refs argument `{}`",
                    String::from_utf8_lossy(value)
                ));
            }
        }
    }
    Ok(request)
}

fn parse_fetch(arguments: &[PktLine]) -> Result<FetchV2Request> {
    let mut request = FetchV2Request {
        wants: Vec::new(),
        haves: Vec::new(),
        done: false,
        ofs_delta: false,
        include_tag: false,
        shallow: Vec::new(),
        depth: None,
        deepen_relative: false,
    };
    for argument in data_lines(arguments)? {
        if let Some(value) = argument.strip_prefix(b"want ") {
            push_id(&mut request.wants, value, "want")?;
        } else if let Some(value) = argument.strip_prefix(b"have ") {
            push_id(&mut request.haves, value, "have")?;
        } else if let Some(value) = argument.strip_prefix(b"shallow ") {
            push_id(&mut request.shallow, value, "shallow")?;
        } else if let Some(value) = argument.strip_prefix(b"deepen ") {
            if request.depth.is_some() {
                return protocol_error("duplicate deepen argument");
            }
            let value = std::str::from_utf8(value)
                .map_err(|_| Error::Protocol("deepen is not ASCII".into()))?;
            let depth = value
                .parse::<usize>()
                .map_err(|_| Error::Protocol("invalid deepen depth".into()))?;
            if depth == 0 {
                return protocol_error("deepen depth must be positive");
            }
            request.depth = Some(depth);
        } else {
            match argument {
                b"deepen-relative" => set_once(&mut request.deepen_relative, "deepen-relative")?,
                b"done" => set_once(&mut request.done, "done")?,
                b"ofs-delta" => set_once(&mut request.ofs_delta, "ofs-delta")?,
                b"include-tag" => set_once(&mut request.include_tag, "include-tag")?,
                b"thin-pack" | b"no-progress" => {}
                value => {
                    return protocol_error(format!(
                        "unsupported fetch argument `{}`",
                        String::from_utf8_lossy(value)
                    ));
                }
            }
        }
    }
    if request.deepen_relative && request.depth.is_none() {
        return protocol_error("deepen-relative requires deepen");
    }
    Ok(request)
}

fn data_lines(arguments: &[PktLine]) -> Result<Vec<&[u8]>> {
    arguments
        .iter()
        .map(|packet| match packet {
            PktLine::Data(value) => Ok(line(value)),
            _ => protocol_error("control packet in protocol v2 command arguments"),
        })
        .collect()
}

fn line(value: &[u8]) -> &[u8] {
    value.strip_suffix(b"\n").unwrap_or(value)
}

fn push_id(output: &mut Vec<ObjectId>, value: &[u8], name: &str) -> Result<()> {
    let value = std::str::from_utf8(value)
        .map_err(|_| Error::Protocol(format!("invalid {name} object ID")))?;
    let id = ObjectId::from_str(value)
        .map_err(|_| Error::Protocol(format!("invalid {name} object ID")))?;
    if output.contains(&id) {
        return Ok(());
    }
    output.push(id);
    Ok(())
}

fn set_once(value: &mut bool, name: &str) -> Result<()> {
    if std::mem::replace(value, true) {
        return protocol_error(format!("duplicate `{name}` argument"));
    }
    Ok(())
}

fn selected(name: &[u8], prefixes: &[Vec<u8>]) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|prefix| name.starts_with(prefix))
}

fn append_v2_ref(
    output: &mut Vec<u8>,
    id: ObjectId,
    name: &[u8],
    symref: Option<&[u8]>,
    peeled: Option<ObjectId>,
) -> Result<()> {
    let mut value = format!("{id} ").into_bytes();
    value.extend_from_slice(name);
    if let Some(target) = symref {
        value.extend_from_slice(b" symref-target:");
        value.extend_from_slice(target);
    }
    if let Some(id) = peeled {
        value.extend_from_slice(format!(" peeled:{id}").as_bytes());
    }
    value.push(b'\n');
    append_packet(output, &value)
}

fn append_unborn(output: &mut Vec<u8>, name: &[u8], symref: Option<&[u8]>) -> Result<()> {
    let mut value = b"unborn ".to_vec();
    value.extend_from_slice(name);
    if let Some(target) = symref {
        value.extend_from_slice(b" symref-target:");
        value.extend_from_slice(target);
    }
    value.push(b'\n');
    append_packet(output, &value)
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
    use super::*;
    use crate::{
        CommitBuilder, EntryMode, InitOptions, MemoryFileSystem, ObjectKind, PreviousValue,
        ReferenceName, Signature, TagBuilder, Tree, TreeEntry,
    };

    fn encode(packets: &[PktLine]) -> Vec<u8> {
        packets
            .iter()
            .flat_map(|packet| packet.encode().unwrap())
            .collect()
    }

    fn command(name: &str, arguments: &[Vec<u8>]) -> Vec<u8> {
        let mut packets = vec![
            PktLine::Data(format!("command={name}\n").into_bytes()),
            PktLine::Data(b"object-format=sha1\n".to_vec()),
            PktLine::Delimiter,
        ];
        packets.extend(arguments.iter().cloned().map(PktLine::Data));
        packets.push(PktLine::Flush);
        encode(&packets)
    }

    fn repository() -> (Repository, ObjectId) {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"v2\n").unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let signature = Signature::new("V2", "v2@example.com", 1, 0).unwrap();
        let tip = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature)
                    .message(b"v2\n".to_vec())
                    .build(),
            )
            .unwrap();
        repository
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        (repository, tip)
    }

    #[test]
    fn advertises_only_implemented_v2_capabilities() {
        let (repository, _) = repository();
        let bytes = repository.advertise_upload_pack_v2().unwrap();
        let packets = decode_request(&bytes).unwrap();
        assert_eq!(packets[0], PktLine::Data(b"version 2\n".to_vec()));
        assert!(packets.contains(&PktLine::Data(b"ls-refs=unborn\n".to_vec())));
        assert!(packets.contains(&PktLine::Data(b"fetch=shallow\n".to_vec())));
        assert_eq!(packets.last(), Some(&PktLine::Flush));
    }

    #[test]
    fn parses_bounded_ls_refs_and_rejects_unknown_arguments() {
        let request = UploadPackV2Request::parse(
            &command(
                "ls-refs",
                &[
                    b"symrefs\n".to_vec(),
                    b"peel\n".to_vec(),
                    b"ref-prefix refs/heads/\n".to_vec(),
                ],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let UploadPackV2Request::LsRefs(request) = request else {
            panic!("expected ls-refs");
        };
        assert!(request.symrefs());
        assert!(request.peel());
        assert_eq!(request.prefixes(), &[b"refs/heads/".to_vec()]);

        assert!(
            UploadPackV2Request::parse(
                &command("ls-refs", &[b"unknown\n".to_vec()]),
                &UploadPackV2Limits::default(),
            )
            .is_err()
        );
        assert!(
            UploadPackV2Request::parse(
                &command("ls-refs", &[b"ref-prefix a\n".to_vec()]),
                &UploadPackV2Limits {
                    max_prefixes: 0,
                    ..UploadPackV2Limits::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn ls_refs_reports_symrefs_prefixes_and_unborn_head() {
        let (repository, tip) = repository();
        let parsed = UploadPackV2Request::parse(
            &command(
                "ls-refs",
                &[b"symrefs\n".to_vec(), b"ref-prefix HEAD\n".to_vec()],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let response = repository
            .respond_upload_pack_v2(
                &parsed,
                &UploadPackOptions::default(),
                &UploadPackV2Limits::default(),
            )
            .unwrap();
        let packets = decode_request(&response).unwrap();
        assert_eq!(
            packets[0],
            PktLine::Data(format!("{tip} HEAD symref-target:refs/heads/main\n").into_bytes())
        );

        let unborn =
            Repository::init(MemoryFileSystem::new(), "empty", &InitOptions::default()).unwrap();
        let parsed = UploadPackV2Request::parse(
            &command("ls-refs", &[b"symrefs\n".to_vec(), b"unborn\n".to_vec()]),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let response = unborn
            .respond_upload_pack_v2(
                &parsed,
                &UploadPackOptions::default(),
                &UploadPackV2Limits::default(),
            )
            .unwrap();
        assert_eq!(
            decode_request(&response).unwrap()[0],
            PktLine::Data(b"unborn HEAD symref-target:refs/heads/main\n".to_vec())
        );
    }

    #[test]
    fn fetch_negotiates_and_returns_sectioned_sideband_pack() {
        let (repository, tip) = repository();
        let negotiation = UploadPackV2Request::parse(
            &command(
                "fetch",
                &[
                    format!("have {tip}\n").into_bytes(),
                    format!("want {tip}\n").into_bytes(),
                ],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let response = repository
            .respond_upload_pack_v2(
                &negotiation,
                &UploadPackOptions::default(),
                &UploadPackV2Limits::default(),
            )
            .unwrap();
        let packets = decode_request(&response).unwrap();
        assert_eq!(packets[0], PktLine::Data(b"acknowledgments\n".to_vec()));
        assert_eq!(
            packets[1],
            PktLine::Data(format!("ACK {tip}\n").into_bytes())
        );

        let fetch = UploadPackV2Request::parse(
            &command(
                "fetch",
                &[
                    format!("want {tip}\n").into_bytes(),
                    b"thin-pack\n".to_vec(),
                    b"no-progress\n".to_vec(),
                    b"ofs-delta\n".to_vec(),
                    b"done\n".to_vec(),
                ],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let response = repository
            .respond_upload_pack_v2(
                &fetch,
                &UploadPackOptions::default(),
                &UploadPackV2Limits::default(),
            )
            .unwrap();
        let packets = decode_request(&response).unwrap();
        assert_eq!(packets[0], PktLine::Data(b"packfile\n".to_vec()));
        let PktLine::Data(sideband) = &packets[1] else {
            panic!("expected sideband pack");
        };
        assert_eq!(sideband.first(), Some(&1));
        assert!(sideband[1..].starts_with(b"PACK"));
        assert_eq!(packets.last(), Some(&PktLine::Flush));
    }

    #[test]
    fn deduplicates_ids_and_rejects_filter_and_bad_framing() {
        let id = ObjectId::from_bytes([1; ObjectId::LENGTH]);
        let duplicate = UploadPackV2Request::parse(
            &command(
                "fetch",
                &[
                    format!("want {id}\n").into_bytes(),
                    format!("want {id}\n").into_bytes(),
                ],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let UploadPackV2Request::Fetch(duplicate) = duplicate else {
            panic!("expected fetch");
        };
        assert_eq!(duplicate.wants(), &[id]);
        for arguments in [
            vec![
                format!("want {id}\n").into_bytes(),
                b"deepen-relative\n".to_vec(),
            ],
            vec![
                format!("want {id}\n").into_bytes(),
                b"filter blob:none\n".to_vec(),
            ],
        ] {
            assert!(
                UploadPackV2Request::parse(
                    &command("fetch", &arguments),
                    &UploadPackV2Limits::default(),
                )
                .is_err()
            );
        }
        assert!(
            UploadPackV2Request::parse(
                &encode(&[PktLine::Data(b"command=fetch\n".to_vec()), PktLine::Flush]),
                &UploadPackV2Limits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn include_tag_adds_complete_nested_annotated_tag_chain() {
        let (repository, tip) = repository();
        let signature = Signature::new("Tag", "tag@example.com", 2, 0).unwrap();
        let inner = TagBuilder::new(tip, ObjectKind::Commit, "inner", signature.clone())
            .unwrap()
            .build();
        let inner_id = repository.write_tag(&inner, 4096).unwrap();
        let outer = TagBuilder::new(inner_id, ObjectKind::Tag, "outer", signature)
            .unwrap()
            .build();
        let outer_id = repository.write_tag(&outer, 4096).unwrap();
        repository
            .update_reference(
                &ReferenceName::new("refs/tags/nested").unwrap(),
                outer_id,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let mut wanted = repository
            .reachable_objects_bounded(&[tip], 4096, false, 100)
            .unwrap();
        repository
            .include_reachable_tags(&mut wanted, 4096, 100, 2)
            .unwrap();
        assert!(wanted.contains(&inner_id));
        assert!(wanted.contains(&outer_id));
        assert!(
            repository
                .include_reachable_tags(&mut vec![tip], 4096, 100, 1)
                .is_err()
        );
    }

    #[test]
    fn shallow_fetch_uses_v2_shallow_info_section() {
        let (repository, tip) = repository();
        let fetch = UploadPackV2Request::parse(
            &command(
                "fetch",
                &[
                    format!("want {tip}\n").into_bytes(),
                    b"deepen 1\n".to_vec(),
                    b"done\n".to_vec(),
                ],
            ),
            &UploadPackV2Limits::default(),
        )
        .unwrap();
        let response = repository
            .respond_upload_pack_v2(
                &fetch,
                &UploadPackOptions::default(),
                &UploadPackV2Limits::default(),
            )
            .unwrap();
        let packets = decode_request(&response).unwrap();
        assert_eq!(packets[0], PktLine::Data(b"shallow-info\n".to_vec()));
        assert_eq!(packets[1], PktLine::Delimiter);
        assert_eq!(packets[2], PktLine::Data(b"packfile\n".to_vec()));
    }
}
