//! Transport-neutral fetch and clone orchestration.

use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use crate::{
    Capability, CheckoutOptions, Error, FileSystem, GraphOptions, IncomingPackOptions, InitOptions,
    ObjectId, ObjectKind, PktLine, PreviousValue, RefSpec, ReferenceEdit, ReferenceName,
    ReferenceTarget, Remote, Repository, Result, Sideband, UploadPackOptions, UploadPackRequest,
};

/// Byte exchange required from an upload-pack transport.
pub trait UploadPackTransport {
    /// Obtain the protocol v0/v1 ref advertisement.
    ///
    /// # Errors
    /// Returns transport, remote, or framing errors.
    fn advertise(&mut self) -> Result<Vec<u8>>;
    /// Send one complete stateless request and return its complete response.
    ///
    /// # Errors
    /// Returns transport or remote service errors.
    fn request(&mut self, request: &[u8]) -> Result<Vec<u8>>;
}

/// In-process transport useful for memory repositories and server embedding.
pub struct RepositoryTransport<'a> {
    repository: &'a Repository,
    options: UploadPackOptions,
}

impl<'a> RepositoryTransport<'a> {
    #[must_use]
    pub const fn new(repository: &'a Repository, options: UploadPackOptions) -> Self {
        Self {
            repository,
            options,
        }
    }
}

impl UploadPackTransport for RepositoryTransport<'_> {
    fn advertise(&mut self) -> Result<Vec<u8>> {
        self.repository.advertise_upload_pack()
    }

    fn request(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        let request = UploadPackRequest::parse(request)?;
        self.repository.respond_upload_pack(&request, &self.options)
    }
}

/// One advertised remote ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRef {
    name: String,
    id: ObjectId,
}

impl RemoteRef {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }
}

/// Parsed upload-pack advertisement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteAdvertisement {
    refs: Vec<RemoteRef>,
    capabilities: Vec<Capability>,
    head_target: Option<String>,
}

impl RemoteAdvertisement {
    /// Parse an advertisement through its terminating flush packet.
    ///
    /// # Errors
    /// Returns an error for malformed framing, object IDs, duplicate refs,
    /// capabilities, symbolic HEAD values, or trailing bytes.
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut cursor = 0;
        let mut refs = Vec::new();
        let mut capabilities = Vec::new();
        let mut names = BTreeSet::new();
        let mut first = true;
        loop {
            let (packet, consumed) = PktLine::decode(&input[cursor..])?;
            cursor = cursor
                .checked_add(consumed)
                .ok_or_else(|| Error::Protocol("advertisement offset overflow".into()))?;
            match packet {
                PktLine::Flush => break,
                PktLine::Data(mut line) => {
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    let capability_bytes = if first {
                        line.iter().position(|byte| *byte == 0).map(|nul| {
                            let values = line.split_off(nul + 1);
                            line.pop();
                            values
                        })
                    } else if line.contains(&0) {
                        return protocol_error("capabilities after first advertised ref");
                    } else {
                        None
                    };
                    let remote_ref = parse_advertised_ref(&line)?;
                    if !names.insert(remote_ref.name.clone()) {
                        return protocol_error(format!(
                            "duplicate advertised ref `{}`",
                            remote_ref.name
                        ));
                    }
                    if let Some(values) = capability_bytes {
                        capabilities = Capability::parse_list(&values)?;
                    }
                    refs.push(remote_ref);
                    first = false;
                }
                PktLine::Delimiter | PktLine::ResponseEnd => {
                    return protocol_error("v2 control packet in v0/v1 advertisement");
                }
            }
        }
        if cursor != input.len() {
            return protocol_error("bytes follow advertisement flush");
        }
        let head_target = capabilities
            .iter()
            .find(|capability| capability.name() == "symref")
            .and_then(Capability::value)
            .and_then(|value| value.strip_prefix("HEAD:"))
            .map(str::to_owned);
        if let Some(target) = &head_target {
            ReferenceName::new(target.clone())?;
        }
        Ok(Self {
            refs,
            capabilities,
            head_target,
        })
    }

    #[must_use]
    pub fn refs(&self) -> &[RemoteRef] {
        &self.refs
    }

    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    #[must_use]
    pub fn head_target(&self) -> Option<&str> {
        self.head_target.as_deref()
    }

    fn default_branch(&self) -> Option<&RemoteRef> {
        if let Some(target) = &self.head_target
            && let Some(reference) = self.refs.iter().find(|reference| &reference.name == target)
        {
            return Some(reference);
        }
        let head = self
            .refs
            .iter()
            .find(|reference| reference.name == "HEAD")?;
        self.refs
            .iter()
            .find(|reference| reference.name.starts_with("refs/heads/") && reference.id == head.id)
    }
}

/// Fetch mapping and resource settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchOptions {
    pub remote_name: String,
    pub fetch_tags: bool,
    pub max_pack_size: usize,
    pub max_object_size: usize,
    pub max_total_inflated_size: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            remote_name: "origin".to_owned(),
            fetch_tags: true,
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Refs and pack resulting from a fetch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchResult {
    pub advertisement: RemoteAdvertisement,
    pub updated_refs: Vec<ReferenceName>,
    pub received_objects: usize,
}

/// Clone initialization, remote configuration, and checkout choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloneOptions {
    pub remote_name: String,
    pub remote_url: String,
    pub bare: bool,
    pub checkout: bool,
    pub max_pack_size: usize,
    pub max_object_size: usize,
    pub max_total_inflated_size: usize,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            remote_name: "origin".to_owned(),
            remote_url: String::new(),
            bare: false,
            checkout: true,
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Fetch all advertised branches and, optionally, tags.
    ///
    /// # Errors
    /// Returns an error for transport failures, malformed protocol data,
    /// invalid remote names, corrupt packs, tag conflicts, or ref failures.
    pub fn fetch<T: UploadPackTransport>(
        &self,
        transport: &mut T,
        options: &FetchOptions,
    ) -> Result<FetchResult> {
        let advertisement = RemoteAdvertisement::parse(&transport.advertise()?)?;
        self.fetch_advertisement(transport, advertisement, options, false, None)
    }

    /// Fetch using one named remote's configured positive and negative
    /// refspecs. The caller supplies the transport corresponding to its URL.
    ///
    /// # Errors
    /// Returns an error for a missing/malformed remote plus the errors from
    /// [`Self::fetch`].
    pub fn fetch_remote<T: UploadPackTransport>(
        &self,
        name: &str,
        transport: &mut T,
        options: &FetchOptions,
    ) -> Result<FetchResult> {
        let remote = self.remote(name)?;
        let advertisement = RemoteAdvertisement::parse(&transport.advertise()?)?;
        let mut configured = options.clone();
        name.clone_into(&mut configured.remote_name);
        self.fetch_advertisement(transport, advertisement, &configured, false, Some(&remote))
    }

    /// Initialize, fetch, configure, and optionally check out a clone.
    ///
    /// # Errors
    /// Returns an error for malformed advertisement, invalid options, storage,
    /// pack validation, ref transaction, or checkout failures.
    pub fn clone_from<F: FileSystem, T: UploadPackTransport>(
        filesystem: F,
        path: impl AsRef<Path>,
        transport: &mut T,
        options: &CloneOptions,
    ) -> Result<(Self, FetchResult)> {
        Self::clone_from_shared(Arc::new(filesystem), path, transport, options)
    }

    /// Clone using a shared dynamically dispatched storage adapter.
    ///
    /// This is useful for nested repositories such as submodules, where the
    /// child must retain the same routing adapter as its superproject.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::clone_from`].
    pub fn clone_from_shared<T: UploadPackTransport>(
        filesystem: Arc<dyn FileSystem>,
        path: impl AsRef<Path>,
        transport: &mut T,
        options: &CloneOptions,
    ) -> Result<(Self, FetchResult)> {
        validate_remote_name(&options.remote_name)?;
        let advertisement = RemoteAdvertisement::parse(&transport.advertise()?)?;
        let default = advertisement.default_branch();
        let initial_branch = default
            .and_then(|reference| reference.name.strip_prefix("refs/heads/"))
            .unwrap_or("main")
            .to_owned();
        let repository = Self::init_shared(
            filesystem,
            path,
            &InitOptions {
                bare: options.bare,
                initial_branch: initial_branch.clone(),
            },
        )?;
        let result = repository.fetch_advertisement(
            transport,
            advertisement,
            &FetchOptions {
                remote_name: options.remote_name.clone(),
                fetch_tags: true,
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
            },
            options.bare,
            None,
        )?;
        repository.write_clone_config(options, &initial_branch)?;
        if let Some(remote_head) = result.advertisement.default_branch() {
            if !options.bare {
                let local = ReferenceName::branch(&initial_branch)?;
                repository.update_reference(&local, remote_head.id, PreviousValue::MustNotExist)?;
            }
            if options.checkout && !options.bare {
                let commit = repository.read_commit(remote_head.id, options.max_object_size)?;
                repository.checkout_tree(
                    commit.tree(),
                    &CheckoutOptions {
                        force: true,
                        max_object_size: options.max_object_size,
                    },
                )?;
            }
        }
        Ok((repository, result))
    }

    fn fetch_advertisement<T: UploadPackTransport>(
        &self,
        transport: &mut T,
        advertisement: RemoteAdvertisement,
        options: &FetchOptions,
        bare_mapping: bool,
        remote: Option<&Remote>,
    ) -> Result<FetchResult> {
        validate_remote_name(&options.remote_name)?;
        validate_sha1_advertisement(&advertisement)?;
        let selected = advertisement
            .refs
            .iter()
            .filter(|reference| {
                remote.map_or_else(
                    || {
                        reference.name.starts_with("refs/heads/")
                            || (options.fetch_tags && reference.name.starts_with("refs/tags/"))
                    },
                    |remote| {
                        selected_by_refspecs(remote.fetch_refspecs(), &reference.name)
                            || (options.fetch_tags && reference.name.starts_with("refs/tags/"))
                    },
                )
            })
            .filter(|reference| !is_peeled_ref(&reference.name))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(FetchResult {
                advertisement,
                updated_refs: Vec::new(),
                received_objects: 0,
            });
        }
        let request = self.build_fetch_request(&selected)?;
        let response = transport.request(&request)?;
        let pack = parse_fetch_response(&response)?;
        let validated = self.validate_incoming_pack(
            &pack,
            &IncomingPackOptions {
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
                use_deltas: true,
            },
        )?;
        let received_objects = validated.object_ids().len();

        self.publish_validated_pack(&validated)?;
        let edits = self.fetch_ref_edits(
            &selected,
            &options.remote_name,
            bare_mapping,
            remote.map(Remote::fetch_refspecs),
            options.max_object_size,
        )?;
        self.apply_reference_transaction(&edits)?;
        let updated_refs = edits.into_iter().map(|edit| edit.name().clone()).collect();
        Ok(FetchResult {
            advertisement,
            updated_refs,
            received_objects,
        })
    }

    fn build_fetch_request(&self, selected: &[&RemoteRef]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut seen = BTreeSet::new();
        for reference in selected {
            if !seen.insert(reference.id) {
                continue;
            }
            let suffix = if output.is_empty() {
                " side-band-64k ofs-delta no-progress object-format=sha1"
            } else {
                ""
            };
            append_packet(
                &mut output,
                format!("want {}{suffix}\n", reference.id).as_bytes(),
            )?;
        }
        output.extend(PktLine::Flush.encode()?);
        for id in self.local_have_ids()? {
            append_packet(&mut output, format!("have {id}\n").as_bytes())?;
        }
        append_packet(&mut output, b"done\n")?;
        Ok(output)
    }

    fn local_have_ids(&self) -> Result<BTreeSet<ObjectId>> {
        let mut ids = BTreeSet::new();
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

    fn fetch_ref_edits(
        &self,
        selected: &[&RemoteRef],
        remote: &str,
        bare_mapping: bool,
        refspecs: Option<&[RefSpec]>,
        max_object_size: usize,
    ) -> Result<Vec<ReferenceEdit>> {
        let mut edits = Vec::new();
        let mut destinations = BTreeSet::new();
        for reference in selected {
            let mappings = if let Some(refspecs) = refspecs {
                let mut mappings = refspecs
                    .iter()
                    .filter(|spec| !spec.is_negative())
                    .filter_map(|spec| {
                        spec.map_destination(&reference.name)
                            .map(|destination| (destination, spec.is_force()))
                    })
                    .collect::<Vec<_>>();
                if mappings.is_empty() && reference.name.starts_with("refs/tags/") {
                    mappings.push((reference.name.clone(), false));
                }
                mappings
            } else {
                let destination = if bare_mapping && reference.name.starts_with("refs/heads/") {
                    reference.name.clone()
                } else if let Some(branch) = reference.name.strip_prefix("refs/heads/") {
                    format!("refs/remotes/{remote}/{branch}")
                } else {
                    reference.name.clone()
                };
                vec![(destination, !reference.name.starts_with("refs/tags/"))]
            };
            for (destination, force) in mappings {
                let destination = ReferenceName::new(destination)?;
                if !destinations.insert(destination.clone()) {
                    return Err(Error::ReferenceConflict(destination.to_string()));
                }
                let previous = match self.resolve_reference(destination.as_str()) {
                    Ok(id) if id == reference.id => continue,
                    Ok(id) if force => PreviousValue::MustExist(id),
                    Ok(id)
                        if fetch_update_is_fast_forward(
                            self,
                            id,
                            reference.id,
                            max_object_size,
                        )? =>
                    {
                        PreviousValue::MustExist(id)
                    }
                    Ok(_) => return Err(Error::ReferenceConflict(destination.to_string())),
                    Err(Error::NotFound(_)) => PreviousValue::MustNotExist,
                    Err(error) => return Err(error),
                };
                edits.push(ReferenceEdit::update(destination, reference.id, previous));
            }
        }
        Ok(edits)
    }

    fn write_clone_config(&self, options: &CloneOptions, branch: &str) -> Result<()> {
        let mut config = self.read_config()?;
        let fetch_destination = if options.bare {
            "refs/heads/*".to_owned()
        } else {
            format!("refs/remotes/{}/*", options.remote_name)
        };
        config.set(
            &format!("remote.{}.url", options.remote_name),
            options.remote_url.as_bytes(),
        )?;
        config.set(
            &format!("remote.{}.fetch", options.remote_name),
            format!("+refs/heads/*:{fetch_destination}"),
        )?;
        if !options.bare {
            config.set(
                &format!("branch.{branch}.remote"),
                options.remote_name.as_bytes(),
            )?;
            config.set(
                &format!("branch.{branch}.merge"),
                format!("refs/heads/{branch}"),
            )?;
        }
        self.write_config(&config)
    }
}

fn parse_advertised_ref(line: &[u8]) -> Result<RemoteRef> {
    if line.len() < ObjectId::HEX_LENGTH + 2 || line[ObjectId::HEX_LENGTH] != b' ' {
        return protocol_error("malformed advertised ref");
    }
    let id = ObjectId::from_str(
        std::str::from_utf8(&line[..ObjectId::HEX_LENGTH])
            .map_err(|_| Error::Protocol("advertised ID is not ASCII".into()))?,
    )
    .map_err(|_| Error::Protocol("invalid advertised object ID".into()))?;
    let name = std::str::from_utf8(&line[ObjectId::HEX_LENGTH + 1..])
        .map_err(|_| Error::Protocol("advertised ref is not UTF-8".into()))?
        .to_owned();
    if name != "HEAD" && name != "capabilities^{}" {
        let validated = name.strip_suffix("^{}").unwrap_or(&name);
        ReferenceName::new(validated.to_owned())?;
    }
    Ok(RemoteRef { name, id })
}

fn is_peeled_ref(name: &str) -> bool {
    name.ends_with("^{}")
}

fn validate_sha1_advertisement(advertisement: &RemoteAdvertisement) -> Result<()> {
    if let Some(format) = advertisement
        .capabilities
        .iter()
        .find(|capability| capability.name() == "object-format")
        .and_then(Capability::value)
        && format != "sha1"
    {
        return protocol_error(format!("unsupported remote object format `{format}`"));
    }
    for required in ["side-band-64k", "ofs-delta"] {
        if !advertisement
            .capabilities
            .iter()
            .any(|capability| capability.name() == required)
        {
            return protocol_error(format!("remote does not support `{required}`"));
        }
    }
    Ok(())
}

fn parse_fetch_response(input: &[u8]) -> Result<Vec<u8>> {
    let (negotiation, mut cursor) = PktLine::decode(input)?;
    let PktLine::Data(line) = negotiation else {
        return protocol_error("fetch response has no ACK/NAK");
    };
    if line != b"NAK\n" && !(line.starts_with(b"ACK ") && line.ends_with(b"\n")) {
        return protocol_error("malformed fetch negotiation response");
    }
    let mut pack = Vec::new();
    loop {
        let (packet, consumed) = PktLine::decode(&input[cursor..])?;
        cursor += consumed;
        match packet {
            PktLine::Data(data) => match Sideband::decode(&data)? {
                Sideband::Data(data) => pack.extend(data),
                Sideband::Progress(_) => {}
                Sideband::Error(error) => {
                    return protocol_error(format!(
                        "remote upload-pack failed: {}",
                        String::from_utf8_lossy(&error)
                    ));
                }
            },
            PktLine::Flush => break,
            PktLine::Delimiter | PktLine::ResponseEnd => {
                return protocol_error("unexpected fetch response control packet");
            }
        }
    }
    if cursor != input.len() || pack.is_empty() {
        return protocol_error("trailing bytes or missing pack in fetch response");
    }
    Ok(pack)
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

fn selected_by_refspecs(refspecs: &[RefSpec], name: &str) -> bool {
    refspecs
        .iter()
        .any(|spec| !spec.is_negative() && spec.matches(name))
        && !refspecs
            .iter()
            .any(|spec| spec.is_negative() && spec.matches(name))
}

fn fetch_update_is_fast_forward(
    repository: &Repository,
    old: ObjectId,
    new: ObjectId,
    max_object_size: usize,
) -> Result<bool> {
    let old_kind = repository.read_object(old, max_object_size)?.kind();
    let new_kind = repository.read_object(new, max_object_size)?.kind();
    if old_kind != ObjectKind::Commit || new_kind != ObjectKind::Commit {
        return Ok(false);
    }
    repository.is_ancestor(
        old,
        new,
        &GraphOptions {
            max_object_size,
            ..GraphOptions::default()
        },
    )
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
    use std::path::Path;

    use super::{CloneOptions, FetchOptions, RemoteAdvertisement, RepositoryTransport};
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, ObjectKind,
        PreviousValue, ReferenceName, Repository, Signature, Tree, TreeEntry, UploadPackOptions,
    };

    #[test]
    fn clones_between_memory_filesystems_and_checks_out_default_branch() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let tip = commit(&remote, None, b"hello from clone\n");
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let fs = MemoryFileSystem::new();
        let (clone, result) = Repository::clone_from(
            fs.clone(),
            "clone",
            &mut transport,
            &CloneOptions {
                remote_url: "memory://remote".to_owned(),
                ..CloneOptions::default()
            },
        )
        .unwrap();

        assert_eq!(clone.resolve_reference("HEAD").unwrap(), tip);
        assert_eq!(
            clone.resolve_reference("refs/remotes/origin/main").unwrap(),
            tip
        );
        assert_eq!(
            fs.read(Path::new("clone/file.txt")).unwrap(),
            b"hello from clone\n"
        );
        let config = clone.read_config().unwrap();
        assert_eq!(
            config.get("remote.origin.url").unwrap().unwrap().value(),
            Some(b"memory://remote".as_slice())
        );
        assert_eq!(
            config.get("remote.origin.fetch").unwrap().unwrap().value(),
            Some(b"+refs/heads/*:refs/remotes/origin/*".as_slice())
        );
        assert!(result.received_objects >= 3);
    }

    #[test]
    fn incremental_fetch_sends_haves_and_receives_only_new_closure() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let base = commit(&remote, None, b"base\n");
        let main = ReferenceName::branch("main").unwrap();
        remote
            .update_reference(&main, base, PreviousValue::MustNotExist)
            .unwrap();
        let mut first_transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let destination_fs = MemoryFileSystem::new();
        let (destination, _) = Repository::clone_from(
            destination_fs,
            "clone",
            &mut first_transport,
            &CloneOptions::default(),
        )
        .unwrap();

        let tip = commit(&remote, Some(base), b"updated\n");
        remote
            .update_reference(&main, tip, PreviousValue::MustExist(base))
            .unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let fetched = destination
            .fetch(&mut transport, &FetchOptions::default())
            .unwrap();
        assert_eq!(
            destination
                .resolve_reference("refs/remotes/origin/main")
                .unwrap(),
            tip
        );
        assert_eq!(fetched.received_objects, 3);
        assert_eq!(
            destination.resolve_reference("refs/heads/main").unwrap(),
            base
        );
    }

    #[test]
    fn named_fetch_honors_positive_negative_and_custom_refspecs() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let main = commit(&remote, None, b"main\n");
        let private = commit(&remote, Some(main), b"private\n");
        let change = commit(&remote, Some(main), b"change\n");
        for (name, id) in [
            ("refs/heads/main", main),
            ("refs/heads/private/secret", private),
            ("refs/changes/123", change),
        ] {
            remote
                .update_reference(
                    &ReferenceName::new(name).unwrap(),
                    id,
                    PreviousValue::MustNotExist,
                )
                .unwrap();
        }

        let destination = Repository::init(
            MemoryFileSystem::new(),
            "destination",
            &InitOptions::default(),
        )
        .unwrap();
        destination
            .add_remote("origin", b"memory://remote")
            .unwrap();
        destination
            .add_remote_fetch_refspec("origin", "^refs/heads/private/*")
            .unwrap();
        destination
            .add_remote_fetch_refspec("origin", "+refs/changes/*:refs/cache/*")
            .unwrap();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        destination
            .fetch_remote("origin", &mut transport, &FetchOptions::default())
            .unwrap();

        assert_eq!(
            destination
                .resolve_reference("refs/remotes/origin/main")
                .unwrap(),
            main
        );
        assert!(
            destination
                .resolve_reference("refs/remotes/origin/private/secret")
                .is_err()
        );
        assert_eq!(
            destination.resolve_reference("refs/cache/123").unwrap(),
            change
        );
    }

    #[test]
    fn bare_clone_maps_all_remote_branches_to_local_branches() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let main_tip = commit(&remote, None, b"main\n");
        let topic_tip = commit(&remote, Some(main_tip), b"topic\n");
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                main_tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        remote
            .update_reference(
                &ReferenceName::branch("topic").unwrap(),
                topic_tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();

        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let (clone, _) = Repository::clone_from(
            MemoryFileSystem::new(),
            "clone.git",
            &mut transport,
            &CloneOptions {
                bare: true,
                checkout: false,
                ..CloneOptions::default()
            },
        )
        .unwrap();

        assert_eq!(
            clone.resolve_reference("refs/heads/main").unwrap(),
            main_tip
        );
        assert_eq!(
            clone.resolve_reference("refs/heads/topic").unwrap(),
            topic_tip
        );
        assert!(clone.resolve_reference("refs/remotes/origin/main").is_err());
        let config = clone.read_config().unwrap();
        assert_eq!(
            config.get("remote.origin.fetch").unwrap().unwrap().value(),
            Some(b"+refs/heads/*:refs/heads/*".as_slice())
        );
        assert!(config.get("branch.main.remote").unwrap().is_none());
    }

    #[test]
    fn rejects_trailing_and_duplicate_advertisement_data() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let mut bytes = remote.advertise_upload_pack().unwrap();
        bytes.push(b'x');
        assert!(RemoteAdvertisement::parse(&bytes).is_err());
    }

    #[test]
    fn accepts_peeled_tag_advertisements() {
        let id = "0123456789012345678901234567890123456789";
        let mut bytes = Vec::new();
        super::append_packet(
            &mut bytes,
            format!("{id} refs/tags/v1\0side-band-64k ofs-delta object-format=sha1\n").as_bytes(),
        )
        .unwrap();
        super::append_packet(&mut bytes, format!("{id} refs/tags/v1^{{}}\n").as_bytes()).unwrap();
        bytes.extend(crate::PktLine::Flush.encode().unwrap());

        let advertisement = RemoteAdvertisement::parse(&bytes).unwrap();
        assert_eq!(advertisement.refs().len(), 2);
        assert_eq!(advertisement.refs()[1].name(), "refs/tags/v1^{}");
    }

    fn commit(
        repository: &Repository,
        parent: Option<crate::ObjectId>,
        contents: &[u8],
    ) -> crate::ObjectId {
        let blob = repository.write_object(ObjectKind::Blob, contents).unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file.txt".to_vec(), blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let identity = Signature::new("Clone", "clone@example.com", 1, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, identity.clone(), identity);
        if let Some(parent) = parent {
            builder = builder.parent(parent);
        }
        repository
            .write_commit(&builder.message(b"commit\n".to_vec()).build())
            .unwrap()
    }
}
