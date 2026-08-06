//! Transport-neutral fetch and clone orchestration.

use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use crate::{
    Capability, CheckoutOptions, Error, FileSystem, GraphOptions, IncomingPackOptions, InitOptions,
    ObjectId, ObjectKind, PktLine, PreviousValue, RefSpec, ReferenceEdit, ReferenceName,
    ReferenceTarget, Remote, Repository, Result, Sideband, UploadPackOptions, UploadPackRequest,
    UploadPackV2Limits, UploadPackV2Request,
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

/// Byte exchange required from a protocol-v2 upload-pack transport.
pub trait UploadPackV2Transport {
    /// Obtain the protocol-v2 capability advertisement.
    ///
    /// # Errors
    /// Returns transport, remote, or framing errors.
    fn advertise_v2(&mut self) -> Result<Vec<u8>>;
    /// Send one complete protocol-v2 command and receive its full response.
    ///
    /// # Errors
    /// Returns transport, remote service, or framing errors.
    fn request_v2(&mut self, request: &[u8]) -> Result<Vec<u8>>;
}

/// In-process protocol-v2 transport for repository embedding and tests.
pub struct RepositoryV2Transport<'a> {
    repository: &'a Repository,
    options: UploadPackOptions,
    limits: UploadPackV2Limits,
}

impl<'a> RepositoryV2Transport<'a> {
    #[must_use]
    pub const fn new(
        repository: &'a Repository,
        options: UploadPackOptions,
        limits: UploadPackV2Limits,
    ) -> Self {
        Self {
            repository,
            options,
            limits,
        }
    }
}

impl UploadPackV2Transport for RepositoryV2Transport<'_> {
    fn advertise_v2(&mut self) -> Result<Vec<u8>> {
        self.repository.advertise_upload_pack_v2()
    }

    fn request_v2(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        let request = UploadPackV2Request::parse(request, &self.limits)?;
        self.repository
            .respond_upload_pack_v2(&request, &self.options, &self.limits)
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
        Self::parse_with_limit(input, 10_000_000)
    }

    /// Parse an advertisement while bounding its number of refs.
    ///
    /// # Errors
    /// Returns the same errors as parse and rejects advertisements containing
    /// more entries than the configured limit.
    pub fn parse_with_limit(input: &[u8], max_refs: usize) -> Result<Self> {
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
                    if refs.len() > max_refs {
                        return protocol_error("advertised ref count exceeds limit");
                    }
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
    pub depth: Option<usize>,
    /// Extend every reached shallow boundary by this many generations.
    pub deepen: Option<usize>,
    pub max_shallow_commits: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            remote_name: "origin".to_owned(),
            fetch_tags: true,
            max_pack_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
            max_total_inflated_size: 2 * 1024 * 1024 * 1024,
            depth: None,
            deepen: None,
            max_shallow_commits: 10_000_000,
        }
    }
}

/// Refs and pack resulting from a fetch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchResult {
    pub advertisement: RemoteAdvertisement,
    pub updated_refs: Vec<ReferenceName>,
    pub received_objects: usize,
    pub shallow_commits: BTreeSet<ObjectId>,
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
    pub depth: Option<usize>,
    pub max_shallow_commits: usize,
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
            depth: None,
            max_shallow_commits: 10_000_000,
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

    /// Fetch all advertised branches and optional tags over Git protocol v2.
    ///
    /// # Errors
    /// Returns an error for capability, `ls-refs`, transport, pack, shallow,
    /// reference transaction, or resource-limit failures.
    pub fn fetch_v2<T: UploadPackV2Transport>(
        &self,
        transport: &mut T,
        options: &FetchOptions,
    ) -> Result<FetchResult> {
        let capabilities = parse_v2_capabilities(&transport.advertise_v2()?)?;
        let advertisement = discover_refs_v2(transport, &capabilities)?;
        self.fetch_advertisement_with(
            advertisement,
            options,
            false,
            None,
            |repository, selected, options, _| {
                exchange_fetch_v2(repository, transport, selected, options, &capabilities)
            },
        )
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

    /// Fetch one configured remote over Git protocol v2.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::fetch_v2`] plus malformed remote
    /// configuration or refspec failures.
    pub fn fetch_remote_v2<T: UploadPackV2Transport>(
        &self,
        name: &str,
        transport: &mut T,
        options: &FetchOptions,
    ) -> Result<FetchResult> {
        let remote = self.remote(name)?;
        let capabilities = parse_v2_capabilities(&transport.advertise_v2()?)?;
        let advertisement = discover_refs_v2(transport, &capabilities)?;
        let mut configured = options.clone();
        name.clone_into(&mut configured.remote_name);
        self.fetch_advertisement_with(
            advertisement,
            &configured,
            false,
            Some(&remote),
            |repository, selected, options, _| {
                exchange_fetch_v2(repository, transport, selected, options, &capabilities)
            },
        )
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
        Self::clone_from_advertisement(
            filesystem,
            path,
            advertisement,
            options,
            |repository, advertisement, fetch_options, bare| {
                repository.fetch_advertisement(transport, advertisement, fetch_options, bare, None)
            },
        )
    }

    /// Initialize, fetch over protocol v2, configure, and optionally check out.
    ///
    /// # Errors
    /// Returns capability, `ls-refs`, transport, initialization, pack,
    /// reference, checkout, shallow, or resource-limit errors.
    pub fn clone_from_v2<F: FileSystem, T: UploadPackV2Transport>(
        filesystem: F,
        path: impl AsRef<Path>,
        transport: &mut T,
        options: &CloneOptions,
    ) -> Result<(Self, FetchResult)> {
        Self::clone_from_shared_v2(Arc::new(filesystem), path, transport, options)
    }

    /// Protocol-v2 clone using a shared dynamically dispatched storage adapter.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::clone_from_v2`].
    pub fn clone_from_shared_v2<T: UploadPackV2Transport>(
        filesystem: Arc<dyn FileSystem>,
        path: impl AsRef<Path>,
        transport: &mut T,
        options: &CloneOptions,
    ) -> Result<(Self, FetchResult)> {
        validate_remote_name(&options.remote_name)?;
        let capabilities = parse_v2_capabilities(&transport.advertise_v2()?)?;
        let advertisement = discover_refs_v2(transport, &capabilities)?;
        Self::clone_from_advertisement(
            filesystem,
            path,
            advertisement,
            options,
            |repository, advertisement, fetch_options, bare| {
                repository.fetch_advertisement_with(
                    advertisement,
                    fetch_options,
                    bare,
                    None,
                    |repository, selected, options, _| {
                        exchange_fetch_v2(repository, transport, selected, options, &capabilities)
                    },
                )
            },
        )
    }

    fn clone_from_advertisement<E>(
        filesystem: Arc<dyn FileSystem>,
        path: impl AsRef<Path>,
        advertisement: RemoteAdvertisement,
        options: &CloneOptions,
        fetch: E,
    ) -> Result<(Self, FetchResult)>
    where
        E: FnOnce(&Repository, RemoteAdvertisement, &FetchOptions, bool) -> Result<FetchResult>,
    {
        let default = advertisement.default_branch();
        let initial_branch = default
            .and_then(|reference| reference.name.strip_prefix("refs/heads/"))
            .or_else(|| {
                advertisement
                    .head_target()
                    .and_then(|target| target.strip_prefix("refs/heads/"))
            })
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
        let result = fetch(
            &repository,
            advertisement,
            &FetchOptions {
                remote_name: options.remote_name.clone(),
                fetch_tags: true,
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
                depth: options.depth,
                deepen: None,
                max_shallow_commits: options.max_shallow_commits,
            },
            options.bare,
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
        self.fetch_advertisement_with(
            advertisement,
            options,
            bare_mapping,
            remote,
            |repository, selected, options, advertisement| {
                let request = repository.build_fetch_request(selected, options, advertisement)?;
                let response = transport.request(&request)?;
                parse_fetch_response(
                    &response,
                    options.depth.is_some() || options.deepen.is_some(),
                )
            },
        )
    }

    fn fetch_advertisement_with<E>(
        &self,
        advertisement: RemoteAdvertisement,
        options: &FetchOptions,
        bare_mapping: bool,
        remote: Option<&Remote>,
        exchange: E,
    ) -> Result<FetchResult>
    where
        E: FnOnce(
            &Repository,
            &[&RemoteRef],
            &FetchOptions,
            &RemoteAdvertisement,
        ) -> Result<ParsedFetchResponse>,
    {
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
                shallow_commits: self.shallow_commits(&crate::ShallowOptions {
                    max_commits: options.max_shallow_commits,
                    max_object_size: options.max_object_size,
                })?,
            });
        }
        let parsed = exchange(self, &selected, options, &advertisement)?;
        let validated = self.validate_incoming_pack(
            &parsed.pack,
            &IncomingPackOptions {
                max_pack_size: options.max_pack_size,
                max_object_size: options.max_object_size,
                max_total_inflated_size: options.max_total_inflated_size,
                use_deltas: true,
            },
        )?;
        let received_objects = validated.object_ids().len();

        self.publish_validated_pack(&validated)?;
        let shallow_options = crate::ShallowOptions {
            max_commits: options.max_shallow_commits,
            max_object_size: options.max_object_size,
        };
        let mut shallow_commits = self.shallow_commits(&shallow_options)?;
        for id in parsed.unshallow {
            shallow_commits.remove(&id);
        }
        shallow_commits.extend(parsed.shallow);
        self.write_shallow_commits(&shallow_commits, &shallow_options)?;
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
            shallow_commits,
        })
    }

    fn build_fetch_request(
        &self,
        selected: &[&RemoteRef],
        options: &FetchOptions,
        advertisement: &RemoteAdvertisement,
    ) -> Result<Vec<u8>> {
        if options.depth == Some(0) {
            return protocol_error("fetch depth must be positive");
        }
        if options.deepen == Some(0) {
            return protocol_error("fetch relative depth must be positive");
        }
        if options.depth.is_some() && options.deepen.is_some() {
            return protocol_error("fetch depth and deepen are mutually exclusive");
        }
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_shallow_commits,
            max_object_size: options.max_object_size,
        })?;
        if (options.depth.is_some() || options.deepen.is_some() || !shallow.is_empty())
            && !advertisement
                .capabilities
                .iter()
                .any(|capability| capability.name() == "shallow")
        {
            return protocol_error("remote does not support shallow fetches");
        }
        if options.deepen.is_some()
            && !advertisement
                .capabilities
                .iter()
                .any(|capability| capability.name() == "deepen-relative")
        {
            return protocol_error("remote does not support relative deepening");
        }
        let mut output = Vec::new();
        let mut seen = BTreeSet::new();
        for reference in selected {
            if !seen.insert(reference.id) {
                continue;
            }
            let suffix = if output.is_empty() {
                if options.depth.is_some() || options.deepen.is_some() || !shallow.is_empty() {
                    if options.deepen.is_some() {
                        " side-band-64k ofs-delta no-progress shallow deepen-relative object-format=sha1"
                    } else {
                        " side-band-64k ofs-delta no-progress shallow object-format=sha1"
                    }
                } else {
                    " side-band-64k ofs-delta no-progress object-format=sha1"
                }
            } else {
                ""
            };
            append_packet(
                &mut output,
                format!("want {}{suffix}\n", reference.id).as_bytes(),
            )?;
        }
        for id in shallow {
            append_packet(&mut output, format!("shallow {id}\n").as_bytes())?;
        }
        if let Some(depth) = options.depth {
            append_packet(&mut output, format!("deepen {depth}\n").as_bytes())?;
        }
        if let Some(deepen) = options.deepen {
            append_packet(&mut output, format!("deepen {deepen}\n").as_bytes())?;
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

struct ParsedFetchResponse {
    pack: Vec<u8>,
    shallow: BTreeSet<ObjectId>,
    unshallow: BTreeSet<ObjectId>,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct V2Capabilities {
    shallow: bool,
    object_format: bool,
    server_options: bool,
    ls_refs: bool,
}

pub(crate) fn parse_v2_capabilities(input: &[u8]) -> Result<V2Capabilities> {
    parse_v2_capabilities_for(input, true)
}

fn parse_v2_capabilities_for(input: &[u8], require_fetch: bool) -> Result<V2Capabilities> {
    let packets = decode_all_packets(input)?;
    if packets.first() != Some(&PktLine::Data(b"version 2\n".to_vec()))
        || packets.last() != Some(&PktLine::Flush)
    {
        return protocol_error("invalid protocol v2 capability advertisement");
    }
    let mut object_format = None;
    let mut fetch = None;
    let mut server_options = false;
    let mut ls_refs = false;
    for packet in &packets[1..packets.len() - 1] {
        let PktLine::Data(line) = packet else {
            return protocol_error("control packet in v2 capability advertisement");
        };
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"object-format=") {
            if object_format.replace(value).is_some() {
                return protocol_error("duplicate v2 object-format capability");
            }
        } else if line == b"server-option" {
            if std::mem::replace(&mut server_options, true) {
                return protocol_error("duplicate v2 server-option capability");
            }
        } else if line == b"ls-refs" || line.starts_with(b"ls-refs=") {
            if std::mem::replace(&mut ls_refs, true) {
                return protocol_error("duplicate v2 ls-refs capability");
            }
        } else if (line == b"fetch" || line.starts_with(b"fetch="))
            && fetch
                .replace(line.strip_prefix(b"fetch=").unwrap_or_default())
                .is_some()
        {
            return protocol_error("duplicate v2 fetch capability");
        }
    }
    if object_format.is_some_and(|format| format != b"sha1") {
        return protocol_error("protocol v2 remote does not use SHA-1");
    }
    if require_fetch && fetch.is_none() {
        return protocol_error("remote does not advertise v2 fetch");
    }
    let fetch = fetch.unwrap_or_default();
    Ok(V2Capabilities {
        shallow: fetch
            .split(|byte| *byte == b' ')
            .any(|value| value == b"shallow"),
        object_format: object_format.is_some(),
        server_options,
        ls_refs,
    })
}

fn discover_refs_v2<T: UploadPackV2Transport>(
    transport: &mut T,
    capabilities: &V2Capabilities,
) -> Result<RemoteAdvertisement> {
    let mut request = Vec::new();
    append_packet(&mut request, b"command=ls-refs\n")?;
    if capabilities.object_format {
        append_packet(&mut request, b"object-format=sha1\n")?;
    }
    request.extend(PktLine::Delimiter.encode()?);
    for argument in [
        b"symrefs\n".as_slice(),
        b"peel\n",
        b"unborn\n",
        b"ref-prefix HEAD\n",
        b"ref-prefix refs/heads/\n",
        b"ref-prefix refs/tags/\n",
    ] {
        append_packet(&mut request, argument)?;
    }
    request.extend(PktLine::Flush.encode()?);
    parse_ls_refs_v2(&transport.request_v2(&request)?, capabilities, 10_000_000)
}

pub(crate) fn query_refs_v2<T: UploadPackV2Transport>(
    transport: &mut T,
    prefixes: &[String],
    server_options: &[String],
    max_refs: usize,
    max_response_size: usize,
) -> Result<RemoteAdvertisement> {
    let advertisement = transport.advertise_v2()?;
    if advertisement.len() > max_response_size {
        return protocol_error("v2 capability advertisement exceeds limit");
    }
    let capabilities = parse_v2_capabilities_for(&advertisement, false)?;
    if !capabilities.ls_refs {
        return protocol_error("remote does not advertise v2 ls-refs");
    }
    if !server_options.is_empty() && !capabilities.server_options {
        return protocol_error("remote does not advertise v2 server-option");
    }
    let mut request = Vec::new();
    append_packet(&mut request, b"command=ls-refs\n")?;
    if capabilities.object_format {
        append_packet(&mut request, b"object-format=sha1\n")?;
    }
    for option in server_options {
        if option
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, 0 | b'\n'))
        {
            return protocol_error("v2 server option contains NUL or LF");
        }
        append_packet(&mut request, format!("server-option={option}\n").as_bytes())?;
    }
    request.extend(PktLine::Delimiter.encode()?);
    for argument in [b"symrefs\n".as_slice(), b"peel\n", b"unborn\n"] {
        append_packet(&mut request, argument)?;
    }
    for prefix in prefixes {
        if prefix
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, 0 | b'\n'))
        {
            return protocol_error("v2 ref prefix contains NUL or LF");
        }
        append_packet(&mut request, format!("ref-prefix {prefix}\n").as_bytes())?;
    }
    request.extend(PktLine::Flush.encode()?);
    let response = transport.request_v2(&request)?;
    if response.len() > max_response_size {
        return protocol_error("ls-refs response exceeds limit");
    }
    parse_ls_refs_v2(&response, &capabilities, max_refs)
}

fn parse_ls_refs_v2(
    input: &[u8],
    capabilities: &V2Capabilities,
    max_refs: usize,
) -> Result<RemoteAdvertisement> {
    let packets = decode_all_packets(input)?;
    if packets.last() != Some(&PktLine::Flush) {
        return protocol_error("ls-refs response has no terminating flush");
    }
    let mut refs = Vec::new();
    let mut names = BTreeSet::new();
    let mut head_target = None;
    for packet in &packets[..packets.len() - 1] {
        let PktLine::Data(line) = packet else {
            return protocol_error("control packet in ls-refs response");
        };
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"unborn ") {
            let (name, attributes) = split_v2_ref_fields(rest)?;
            validate_v2_ref_name(name)?;
            validate_v2_ref_attributes(attributes)?;
            if !names.insert(name.to_vec()) {
                return protocol_error("duplicate ls-refs name");
            }
            if names.len() > max_refs {
                return protocol_error("ls-refs ref count exceeds limit");
            }
            if name == b"HEAD" {
                head_target = parse_symref_target(attributes)?;
            }
            continue;
        }
        if line.len() < ObjectId::HEX_LENGTH + 2 || line[ObjectId::HEX_LENGTH] != b' ' {
            return protocol_error("malformed ls-refs line");
        }
        let id = parse_wire_id(&line[..ObjectId::HEX_LENGTH], "ls-refs")?;
        let (name, attributes) = split_v2_ref_fields(&line[ObjectId::HEX_LENGTH + 1..])?;
        validate_v2_ref_name(name)?;
        if !names.insert(name.to_vec()) {
            return protocol_error("duplicate ls-refs name");
        }
        if names.len() > max_refs {
            return protocol_error("ls-refs ref count exceeds limit");
        }
        validate_v2_ref_attributes(attributes)?;
        if name == b"HEAD" {
            head_target = parse_symref_target(attributes)?;
        }
        refs.push(RemoteRef {
            name: std::str::from_utf8(name)
                .map_err(|_| Error::Protocol("ls-refs name is not UTF-8".into()))?
                .to_owned(),
            id,
        });
        if let Some(peeled) = attributes
            .split(|byte| *byte == b' ')
            .find_map(|attribute| attribute.strip_prefix(b"peeled:"))
        {
            refs.push(RemoteRef {
                name: format!(
                    "{}^{{}}",
                    std::str::from_utf8(name)
                        .map_err(|_| Error::Protocol("ls-refs name is not UTF-8".into()))?
                ),
                id: parse_wire_id(peeled, "peeled")?,
            });
        }
        if refs.len() > max_refs {
            return protocol_error("ls-refs ref count exceeds limit");
        }
    }
    let mut effective = b"side-band-64k ofs-delta object-format=sha1".to_vec();
    if capabilities.shallow {
        effective.extend_from_slice(b" shallow deepen-relative");
    }
    Ok(RemoteAdvertisement {
        refs,
        capabilities: Capability::parse_list(&effective)?,
        head_target,
    })
}

fn split_v2_ref_fields(value: &[u8]) -> Result<(&[u8], &[u8])> {
    let split = value
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(value.len());
    let name = &value[..split];
    if name.is_empty() {
        return protocol_error("empty ls-refs name");
    }
    Ok((name, value.get(split + 1..).unwrap_or_default()))
}

fn validate_v2_ref_name(name: &[u8]) -> Result<()> {
    let name = std::str::from_utf8(name)
        .map_err(|_| Error::Protocol("ls-refs name is not UTF-8".into()))?;
    if name != "HEAD" {
        ReferenceName::new(name.to_owned())?;
    }
    Ok(())
}

fn parse_symref_target(attributes: &[u8]) -> Result<Option<String>> {
    for attribute in attributes.split(|byte| *byte == b' ') {
        if let Some(target) = attribute.strip_prefix(b"symref-target:") {
            let target = std::str::from_utf8(target)
                .map_err(|_| Error::Protocol("symref target is not UTF-8".into()))?;
            ReferenceName::new(target.to_owned())?;
            return Ok(Some(target.to_owned()));
        }
    }
    Ok(None)
}

fn validate_v2_ref_attributes(attributes: &[u8]) -> Result<()> {
    for attribute in attributes
        .split(|byte| *byte == b' ')
        .filter(|value| !value.is_empty())
    {
        if let Some(id) = attribute.strip_prefix(b"peeled:") {
            parse_wire_id(id, "peeled")?;
        } else if let Some(target) = attribute.strip_prefix(b"symref-target:") {
            let target = std::str::from_utf8(target)
                .map_err(|_| Error::Protocol("symref target is not UTF-8".into()))?;
            ReferenceName::new(target.to_owned())?;
        } else {
            return protocol_error("unsupported ls-refs attribute");
        }
    }
    Ok(())
}

fn exchange_fetch_v2<T: UploadPackV2Transport>(
    repository: &Repository,
    transport: &mut T,
    selected: &[&RemoteRef],
    options: &FetchOptions,
    capabilities: &V2Capabilities,
) -> Result<ParsedFetchResponse> {
    let request = build_fetch_request_v2(repository, selected, options, capabilities)?;
    parse_fetch_response_v2(&transport.request_v2(&request)?)
}

fn build_fetch_request_v2(
    repository: &Repository,
    selected: &[&RemoteRef],
    options: &FetchOptions,
    capabilities: &V2Capabilities,
) -> Result<Vec<u8>> {
    validate_depth_options(options)?;
    let shallow = repository.shallow_commits(&crate::ShallowOptions {
        max_commits: options.max_shallow_commits,
        max_object_size: options.max_object_size,
    })?;
    if (options.depth.is_some() || options.deepen.is_some() || !shallow.is_empty())
        && !capabilities.shallow
    {
        return protocol_error("protocol v2 remote does not support shallow fetches");
    }
    let mut request = Vec::new();
    append_packet(&mut request, b"command=fetch\n")?;
    if capabilities.object_format {
        append_packet(&mut request, b"object-format=sha1\n")?;
    }
    request.extend(PktLine::Delimiter.encode()?);
    let mut wants = BTreeSet::new();
    for reference in selected {
        if wants.insert(reference.id) {
            append_packet(&mut request, format!("want {}\n", reference.id).as_bytes())?;
        }
    }
    for argument in [b"thin-pack\n".as_slice(), b"no-progress\n", b"ofs-delta\n"] {
        append_packet(&mut request, argument)?;
    }
    for id in shallow {
        append_packet(&mut request, format!("shallow {id}\n").as_bytes())?;
    }
    if let Some(depth) = options.depth {
        append_packet(&mut request, format!("deepen {depth}\n").as_bytes())?;
    }
    if let Some(deepen) = options.deepen {
        append_packet(&mut request, format!("deepen {deepen}\n").as_bytes())?;
        append_packet(&mut request, b"deepen-relative\n")?;
    }
    for id in repository.local_have_ids()? {
        append_packet(&mut request, format!("have {id}\n").as_bytes())?;
    }
    append_packet(&mut request, b"done\n")?;
    request.extend(PktLine::Flush.encode()?);
    Ok(request)
}

fn validate_depth_options(options: &FetchOptions) -> Result<()> {
    if options.depth == Some(0) || options.deepen == Some(0) {
        return protocol_error("fetch depth must be positive");
    }
    if options.depth.is_some() && options.deepen.is_some() {
        return protocol_error("fetch depth and deepen are mutually exclusive");
    }
    Ok(())
}

fn parse_fetch_response_v2(input: &[u8]) -> Result<ParsedFetchResponse> {
    let packets = decode_all_packets(input)?;
    let mut index = 0usize;
    let mut shallow = BTreeSet::new();
    let mut unshallow = BTreeSet::new();
    let mut pack = Vec::new();
    while index < packets.len() {
        match &packets[index] {
            PktLine::Data(header) if header == b"shallow-info\n" => {
                index += 1;
                while let Some(PktLine::Data(line)) = packets.get(index) {
                    parse_shallow_update(line, &mut shallow, &mut unshallow)?;
                    index += 1;
                }
                if packets.get(index) != Some(&PktLine::Delimiter) {
                    return protocol_error("shallow-info has no delimiter");
                }
                index += 1;
            }
            PktLine::Data(header) if header == b"acknowledgments\n" => {
                index += 1;
                while let Some(PktLine::Data(line)) = packets.get(index) {
                    if line != b"NAK\n"
                        && line != b"ready\n"
                        && !(line.starts_with(b"ACK ") && line.ends_with(b"\n"))
                    {
                        return protocol_error("invalid v2 acknowledgment");
                    }
                    index += 1;
                }
                if packets.get(index) != Some(&PktLine::Delimiter) {
                    return protocol_error("acknowledgments has no delimiter");
                }
                index += 1;
            }
            PktLine::Data(header) if header == b"packfile\n" => {
                index += 1;
                while let Some(packet) = packets.get(index) {
                    match packet {
                        PktLine::Data(data) => match Sideband::decode(data)? {
                            Sideband::Data(data) => pack.extend(data),
                            Sideband::Progress(_) => {}
                            Sideband::Error(error) => {
                                return protocol_error(format!(
                                    "remote upload-pack failed: {}",
                                    String::from_utf8_lossy(&error)
                                ));
                            }
                        },
                        PktLine::Flush | PktLine::ResponseEnd => {
                            index += 1;
                            break;
                        }
                        PktLine::Delimiter => {
                            return protocol_error("delimiter inside v2 packfile");
                        }
                    }
                    index += 1;
                }
                break;
            }
            PktLine::Flush | PktLine::ResponseEnd if index + 1 == packets.len() => break,
            _ => return protocol_error("unexpected protocol v2 fetch section"),
        }
    }
    if pack.is_empty() || index != packets.len() {
        return protocol_error("missing v2 pack or trailing response packets");
    }
    Ok(ParsedFetchResponse {
        pack,
        shallow,
        unshallow,
    })
}

fn parse_shallow_update(
    line: &[u8],
    shallow: &mut BTreeSet<ObjectId>,
    unshallow: &mut BTreeSet<ObjectId>,
) -> Result<()> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let (set, value) = if let Some(value) = line.strip_prefix(b"shallow ") {
        (&mut *shallow, value)
    } else if let Some(value) = line.strip_prefix(b"unshallow ") {
        (&mut *unshallow, value)
    } else {
        return protocol_error("invalid v2 shallow update");
    };
    let id = parse_wire_id(value, "shallow update")?;
    if !set.insert(id) || shallow.contains(&id) && unshallow.contains(&id) {
        return protocol_error("duplicate or contradictory v2 shallow update");
    }
    Ok(())
}

fn parse_wire_id(value: &[u8], context: &str) -> Result<ObjectId> {
    if value.len() != ObjectId::HEX_LENGTH || !value.is_ascii() {
        return protocol_error(format!("invalid {context} object ID"));
    }
    ObjectId::from_str(
        std::str::from_utf8(value)
            .map_err(|_| Error::Protocol(format!("invalid {context} object ID")))?,
    )
    .map_err(|_| Error::Protocol(format!("invalid {context} object ID")))
}

fn decode_all_packets(input: &[u8]) -> Result<Vec<PktLine>> {
    let mut decoder = crate::PktLineDecoder::new();
    decoder.extend(input);
    let mut packets = Vec::new();
    while let Some(packet) = decoder.next_packet()? {
        packets.push(packet);
    }
    decoder.finish()?;
    Ok(packets)
}

fn parse_fetch_response(input: &[u8], depth_requested: bool) -> Result<ParsedFetchResponse> {
    let mut cursor = 0usize;
    let mut shallow = BTreeSet::new();
    let mut unshallow = BTreeSet::new();
    if depth_requested {
        loop {
            let (packet, consumed) = PktLine::decode(&input[cursor..])?;
            cursor = cursor
                .checked_add(consumed)
                .ok_or_else(|| Error::Protocol("shallow response offset overflow".into()))?;
            match packet {
                PktLine::Flush => break,
                PktLine::Data(mut line) => {
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    let (set, value, name) = if let Some(value) = line.strip_prefix(b"shallow ") {
                        (&mut shallow, value, "shallow")
                    } else if let Some(value) = line.strip_prefix(b"unshallow ") {
                        (&mut unshallow, value, "unshallow")
                    } else {
                        return protocol_error("invalid shallow update response");
                    };
                    let value = std::str::from_utf8(value)
                        .map_err(|_| Error::Protocol(format!("{name} ID is not ASCII")))?;
                    let id = ObjectId::from_str(value)
                        .map_err(|_| Error::Protocol(format!("invalid {name} ID")))?;
                    if !set.insert(id) || shallow.contains(&id) && unshallow.contains(&id) {
                        return protocol_error("duplicate or contradictory shallow update");
                    }
                }
                PktLine::Delimiter | PktLine::ResponseEnd => {
                    return protocol_error("control packet in shallow update response");
                }
            }
        }
    }
    let (negotiation, consumed) = PktLine::decode(&input[cursor..])?;
    cursor = cursor
        .checked_add(consumed)
        .ok_or_else(|| Error::Protocol("fetch response offset overflow".into()))?;
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
    Ok(ParsedFetchResponse {
        pack,
        shallow,
        unshallow,
    })
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
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::{
        CloneOptions, FetchOptions, RemoteAdvertisement, RepositoryTransport,
        RepositoryV2Transport, parse_fetch_response_v2, parse_v2_capabilities,
    };
    use crate::{
        CommitBuilder, EntryMode, FileSystem, InitOptions, MemoryFileSystem, ObjectKind, PktLine,
        PreviousValue, ReferenceName, ReferenceTarget, Repository, RevisionWalkOptions, Sideband,
        Signature, Tree, TreeEntry, UploadPackOptions, UploadPackV2Limits,
    };

    #[test]
    fn protocol_v2_client_validates_capabilities_and_section_framing() {
        let advertisement = [
            PktLine::Data(b"version 2\n".to_vec()),
            PktLine::Data(b"fetch=shallow\n".to_vec()),
            PktLine::Data(b"object-format=sha1\n".to_vec()),
            PktLine::Flush,
        ]
        .into_iter()
        .flat_map(|packet| packet.encode().unwrap())
        .collect::<Vec<_>>();
        assert!(parse_v2_capabilities(&advertisement).unwrap().shallow);
        assert!(parse_v2_capabilities(&advertisement[..advertisement.len() - 1]).is_err());

        let response = [
            PktLine::Data(b"acknowledgments\n".to_vec()),
            PktLine::Data(format!("ACK {}\n", crate::ObjectId::null()).into_bytes()),
            PktLine::Data(b"ready\n".to_vec()),
            PktLine::Delimiter,
            PktLine::Data(b"packfile\n".to_vec()),
            PktLine::Data(Sideband::Data(b"PACK".to_vec()).encode().unwrap()[4..].to_vec()),
            PktLine::Flush,
        ]
        .into_iter()
        .flat_map(|packet| packet.encode().unwrap())
        .collect::<Vec<_>>();
        assert_eq!(parse_fetch_response_v2(&response).unwrap().pack, b"PACK");
    }

    #[test]
    fn protocol_v2_clone_preserves_an_unborn_remote_head_branch() {
        let remote = Repository::init(
            MemoryFileSystem::new(),
            "remote",
            &InitOptions {
                bare: false,
                initial_branch: "trunk".to_owned(),
            },
        )
        .unwrap();
        let mut transport = RepositoryV2Transport::new(
            &remote,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );

        let (clone, result) = Repository::clone_from_v2(
            MemoryFileSystem::new(),
            "clone",
            &mut transport,
            &CloneOptions::default(),
        )
        .unwrap();

        assert!(result.updated_refs.is_empty());
        assert_eq!(
            clone.read_reference("HEAD").unwrap().target(),
            &ReferenceTarget::Symbolic(ReferenceName::branch("trunk").unwrap())
        );
    }

    #[test]
    fn protocol_v2_fetch_discovers_refs_and_deepens_shallow_history() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let root = commit(&remote, None, b"root\n");
        let tip = commit(&remote, Some(root), b"tip\n");
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let destination_fs = MemoryFileSystem::new();
        let mut transport = RepositoryV2Transport::new(
            &remote,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        let (destination, first) = Repository::clone_from_v2(
            destination_fs.clone(),
            "destination",
            &mut transport,
            &CloneOptions {
                depth: Some(1),
                ..CloneOptions::default()
            },
        )
        .unwrap();
        assert_eq!(first.advertisement.head_target(), Some("refs/heads/main"));
        assert_eq!(first.shallow_commits, BTreeSet::from([tip]));
        assert_eq!(
            destination
                .resolve_reference("refs/remotes/origin/main")
                .unwrap(),
            tip
        );
        assert!(destination.read_commit(root, 4096).is_err());
        assert_eq!(
            destination_fs
                .read(Path::new("destination/file.txt"))
                .unwrap(),
            b"tip\n"
        );

        let second = destination
            .fetch_v2(
                &mut transport,
                &FetchOptions {
                    deepen: Some(1),
                    ..FetchOptions::default()
                },
            )
            .unwrap();
        assert!(second.shallow_commits.is_empty());
        assert_eq!(destination.read_commit(root, 4096).unwrap().parents(), &[]);
    }

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
    fn shallow_clone_and_deepen_persist_git_boundaries() {
        let remote =
            Repository::init(MemoryFileSystem::new(), "remote", &InitOptions::default()).unwrap();
        let root = commit(&remote, None, b"root\n");
        let middle = commit(&remote, Some(root), b"middle\n");
        let tip = commit(&remote, Some(middle), b"tip\n");
        remote
            .update_reference(
                &ReferenceName::branch("main").unwrap(),
                tip,
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let destination_fs = MemoryFileSystem::new();
        let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
        let (destination, result) = Repository::clone_from(
            destination_fs.clone(),
            "clone",
            &mut transport,
            &CloneOptions {
                depth: Some(1),
                ..CloneOptions::default()
            },
        )
        .unwrap();
        assert_eq!(result.shallow_commits, BTreeSet::from([tip]));
        assert!(destination.read_commit(middle, 4096).is_err());
        assert_eq!(
            destination
                .walk_revisions(&[tip], &[], &RevisionWalkOptions::default())
                .unwrap()
                .len(),
            1
        );
        destination.fsck(&crate::FsckOptions::default()).unwrap();
        assert_eq!(
            destination_fs
                .read(Path::new("clone/.git/shallow"))
                .unwrap(),
            format!("{tip}\n").as_bytes()
        );

        let result = destination
            .fetch(
                &mut transport,
                &FetchOptions {
                    deepen: Some(1),
                    ..FetchOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.shallow_commits, BTreeSet::from([middle]));
        assert_eq!(
            destination.read_commit(middle, 4096).unwrap().parents(),
            &[root]
        );
        assert!(destination.read_commit(root, 4096).is_err());

        let result = destination
            .fetch(
                &mut transport,
                &FetchOptions {
                    deepen: Some(1),
                    ..FetchOptions::default()
                },
            )
            .unwrap();
        assert!(result.shallow_commits.is_empty());
        assert_eq!(destination.read_commit(root, 4096).unwrap().parents(), &[]);
        assert!(
            !destination_fs
                .exists(Path::new("clone/.git/shallow"))
                .unwrap()
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
