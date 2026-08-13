//! Git protocol v0/v1 upload-pack advertisement and response generation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;

use crate::{
    Capability, EntryMode, Error, ObjectId, ObjectKind, PktLine, PktLineDecoder, ReferenceTarget,
    Repository, Result, Sideband,
};

const CAPABILITIES: &str = "side-band-64k thin-pack ofs-delta no-progress shallow deepen-relative object-format=sha1 agent=git-rs/0.1";

/// Limits and encoding choices for an upload-pack session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPackOptions {
    pub max_object_size: usize,
    pub max_objects: usize,
    pub max_tag_depth: usize,
    pub use_deltas: bool,
}

pub(crate) struct UploadPackCommon {
    pub(crate) commits: HashSet<ObjectId>,
    pub(crate) valid_haves: HashSet<ObjectId>,
    trees: Vec<ObjectId>,
    commit_trees: BTreeMap<ObjectId, ObjectId>,
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
    shallow: Vec<ObjectId>,
    depth: Option<usize>,
    deepen_relative: bool,
    done: bool,
}

impl UploadPackRequest {
    /// Parse a complete request containing a want section, flush, and
    /// negotiation section.
    ///
    /// # Errors
    /// Returns an error for malformed framing, invalid commands, capabilities
    /// outside the supported advertisement, or data following `done`.
    #[allow(clippy::too_many_lines)]
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut decoder = PktLineDecoder::new();
        decoder.extend(input);
        let mut wants = Vec::new();
        let mut haves = Vec::new();
        let mut capabilities = Vec::new();
        let mut shallow = Vec::new();
        let mut depth = None;
        let mut deepen_relative = false;
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
                        if line.starts_with(b"want ") {
                            let (id, requested) = parse_want(&line, wants.is_empty())?;
                            if wants.contains(&id) {
                                return protocol_error(format!("duplicate want {id}"));
                            }
                            if wants.is_empty() {
                                validate_capabilities(&requested)?;
                                capabilities = requested;
                            } else if !requested.is_empty() {
                                return protocol_error(
                                    "capabilities are only valid on the first want",
                                );
                            }
                            wants.push(id);
                        } else if let Some(value) = line.strip_prefix(b"shallow ") {
                            if wants.is_empty() {
                                return protocol_error("shallow precedes want");
                            }
                            let id = parse_exact_id(value, "shallow")?;
                            if shallow.contains(&id) {
                                return protocol_error(format!("duplicate shallow {id}"));
                            }
                            shallow.push(id);
                        } else if let Some(value) = line.strip_prefix(b"deepen ") {
                            if wants.is_empty() || depth.is_some() {
                                return protocol_error("invalid or duplicate deepen");
                            }
                            let value = std::str::from_utf8(value)
                                .map_err(|_| Error::Protocol("deepen is not ASCII".into()))?;
                            let parsed = value
                                .parse::<usize>()
                                .map_err(|_| Error::Protocol("invalid deepen depth".into()))?;
                            if parsed == 0 {
                                return protocol_error("deepen depth must be positive");
                            }
                            depth = Some(parsed);
                        } else {
                            return protocol_error("unexpected command in want section");
                        }
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
        deepen_relative |= capabilities
            .iter()
            .any(|capability| capability.name() == "deepen-relative");
        if deepen_relative && depth.is_none() {
            return protocol_error("deepen-relative requires deepen");
        }
        Ok(Self {
            wants,
            haves,
            capabilities,
            shallow,
            depth,
            deepen_relative,
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
    pub fn shallow(&self) -> &[ObjectId] {
        &self.shallow
    }

    #[must_use]
    pub const fn depth(&self) -> Option<usize> {
        self.depth
    }

    #[must_use]
    pub const fn is_deepen_relative(&self) -> bool {
        self.deepen_relative
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
            if reference.name().starts_with("refs/tags/") {
                let peeled = reference.peeled().map_or_else(
                    || {
                        self.peel_tag(id, 64, 1024 * 1024 * 1024)
                            .map(|peeled| peeled.id)
                    },
                    Ok,
                )?;
                if peeled != id {
                    advertised.push((format!("{}^{{}}", reference.name()), peeled));
                }
            }
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
    #[allow(clippy::too_many_lines)]
    pub fn respond_upload_pack(
        &self,
        request: &UploadPackRequest,
        options: &UploadPackOptions,
    ) -> Result<Vec<u8>> {
        let tips = self.advertised_tip_ids()?;
        if let Some(id) = request.wants.iter().find(|id| !tips.contains(id)) {
            return protocol_error(format!("want {id} is not an advertised ref"));
        }

        let mut client_shallow = request.shallow.iter().copied().collect::<BTreeSet<_>>();
        client_shallow.extend(self.shallow_commits(&crate::ShallowOptions {
            max_commits: options.max_objects,
            max_object_size: options.max_object_size,
        })?);
        let common = self.reachable_commits(
            &request.haves,
            options.max_object_size,
            options.max_objects,
            &client_shallow,
        )?;
        let acknowledged = request
            .haves
            .iter()
            .rev()
            .find(|id| common.valid_haves.contains(id));
        let mut response = Vec::new();
        let wanted = if let Some(depth) = request.depth {
            let (wanted, boundaries, unshallow) = if request.deepen_relative {
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
            };
            for id in &boundaries {
                append_packet(&mut response, format!("shallow {id}\n").as_bytes())?;
            }
            for id in &unshallow {
                append_packet(&mut response, format!("unshallow {id}\n").as_bytes())?;
            }
            response.extend(PktLine::Flush.encode()?);
            wanted
        } else {
            Vec::new()
        };
        let negotiation = acknowledged.map_or_else(
            || b"NAK\n".to_vec(),
            |id| format!("ACK {id}\n").into_bytes(),
        );
        append_packet(&mut response, &negotiation)?;
        if !request.done {
            return Ok(response);
        }

        let wanted = if request.depth.is_some() {
            wanted
        } else {
            self.select_upload_objects(
                &request.wants,
                &common,
                options.max_object_size,
                options.max_objects,
            )?
        };
        let pack_ids = wanted
            .into_iter()
            .filter(|id| !common.commits.contains(id))
            .collect::<Vec<_>>();
        let external_bases = if request.has_capability("thin-pack") {
            self.upload_external_bases(&common, options.max_object_size, options.max_objects)?
        } else {
            HashSet::new()
        };
        let pack = self.build_upload_pack(
            &pack_ids,
            &crate::pack::UploadPackOptions {
                max_object_size: options.max_object_size,
                max_objects: options.max_objects,
                use_deltas: options.use_deltas && request.has_capability("ofs-delta"),
                use_ofs_delta: request.has_capability("ofs-delta"),
                external_bases,
            },
        )?;
        if request.has_capability("side-band-64k") {
            for chunk in pack
                .bytes()
                .chunks(crate::protocol::MAX_PACKET_DATA_LEN - 1)
            {
                response.extend(Sideband::Data(chunk.to_vec()).encode()?);
            }
            response.extend(PktLine::Flush.encode()?);
        } else {
            response.extend_from_slice(pack.bytes());
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

    pub(crate) fn upload_external_bases(
        &self,
        common: &UploadPackCommon,
        max_size: usize,
        max_objects: usize,
    ) -> Result<HashSet<ObjectId>> {
        let mut known = common.commits.clone();
        let mut trees = common.trees.clone();
        while let Some(id) = trees.pop() {
            if !known.insert(id) {
                continue;
            }
            ensure_object_limit(known.len(), max_objects)?;
            let object = self.read_object_for_upload(id, max_size)?;
            if object.kind() != ObjectKind::Tree {
                return Err(Error::InvalidTree(format!("object {id} is not a tree")));
            }
            for entry in crate::Tree::parse(object.data())?.entries() {
                if entry.mode() == EntryMode::Tree {
                    trees.push(entry.id());
                } else if entry.mode() != EntryMode::Gitlink {
                    known.insert(entry.id());
                    ensure_object_limit(known.len(), max_objects)?;
                }
            }
        }
        Ok(known)
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
        let shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: max_objects,
            max_object_size: max_size,
        })?;
        self.reachable_objects_stopping_at(
            roots,
            max_size,
            ignore_missing_roots,
            max_objects,
            &shallow,
        )
    }

    pub(crate) fn reachable_objects_stopping_at(
        &self,
        roots: &[ObjectId],
        max_size: usize,
        ignore_missing_roots: bool,
        max_objects: usize,
        shallow: &BTreeSet<ObjectId>,
    ) -> Result<Vec<ObjectId>> {
        self.reachable_objects_excluding(
            roots,
            max_size,
            ignore_missing_roots,
            max_objects,
            shallow,
            &HashSet::new(),
        )
    }

    pub(crate) fn reachable_commits(
        &self,
        roots: &[ObjectId],
        max_size: usize,
        max_objects: usize,
        shallow: &BTreeSet<ObjectId>,
    ) -> Result<UploadPackCommon> {
        let has_replacements = self.has_active_replacements()?;
        let graph = if has_replacements {
            None
        } else {
            match self.read_commit_graph(max_size, max_objects) {
                Ok(graph) => Some(graph),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            }
        };
        let roots = roots.iter().copied().collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        let mut trees = Vec::new();
        let mut commit_trees = BTreeMap::new();
        let mut stack = roots.iter().copied().collect::<Vec<_>>();
        let mut packed = (!has_replacements)
            .then(|| self.trusted_packed_reader())
            .transpose()?;
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if seen.len() > max_objects {
                return Err(Error::InvalidObject(
                    "reachable commit traversal exceeds limit".into(),
                ));
            }
            if !roots.contains(&id)
                && let Some(entry) = graph.as_ref().and_then(|graph| graph.get(id))
            {
                commit_trees.insert(id, entry.tree());
                if !shallow.contains(&id) {
                    stack.extend(entry.parents());
                }
                continue;
            }
            let object = packed.as_mut().map_or_else(
                || {
                    self.read_object_for_upload(id, max_size)
                        .map(|object| (object.kind(), object.into_data()))
                },
                |packed| {
                    packed.read(id, max_size).or_else(|error| match error {
                        Error::NotFound(_) => self
                            .read_object_for_upload(id, max_size)
                            .map(|object| (object.kind(), object.into_data())),
                        error => Err(error),
                    })
                },
            );
            match object {
                Ok((ObjectKind::Commit, data)) => {
                    if let Some(entry) = graph.as_ref().and_then(|graph| graph.get(id)) {
                        commit_trees.insert(id, entry.tree());
                        if roots.contains(&id) {
                            trees.push(entry.tree());
                        }
                        if !shallow.contains(&id) {
                            stack.extend(entry.parents());
                        }
                    } else {
                        let (tree, parents) = crate::commit::parse_commit_links(&data)?;
                        commit_trees.insert(id, tree);
                        if roots.contains(&id) {
                            trees.push(tree);
                        }
                        if !shallow.contains(&id) {
                            stack.extend(parents);
                        }
                    }
                }
                Ok(_) => {}
                Err(Error::NotFound(_)) => {
                    seen.remove(&id);
                }
                Err(error) => return Err(error),
            }
        }
        let valid_haves = roots.into_iter().filter(|id| seen.contains(id)).collect();
        Ok(UploadPackCommon {
            commits: seen,
            valid_haves,
            trees,
            commit_trees,
        })
    }

    pub(crate) fn select_upload_objects(
        &self,
        roots: &[ObjectId],
        common: &UploadPackCommon,
        max_size: usize,
        max_objects: usize,
    ) -> Result<Vec<ObjectId>> {
        let mut selected = HashSet::new();
        let mut ordered = Vec::new();
        let mut wanted_trees = Vec::new();
        let mut boundary_trees = common.trees.clone();
        let mut stack = roots.iter().rev().copied().collect::<Vec<_>>();
        while let Some(id) = stack.pop() {
            if common.commits.contains(&id) {
                if let Some(tree) = common.commit_trees.get(&id) {
                    boundary_trees.push(*tree);
                }
                continue;
            }
            if selected.contains(&id) {
                continue;
            }
            let object = self.read_object_for_upload(id, max_size)?;
            match object.kind() {
                ObjectKind::Commit => {
                    insert_upload_object(id, &mut selected, &mut ordered, max_objects)?;
                    let (tree, parents) = crate::commit::parse_commit_links(object.data())?;
                    wanted_trees.push(tree);
                    for parent in parents.iter().rev() {
                        stack.push(*parent);
                    }
                }
                ObjectKind::Tag => {
                    insert_upload_object(id, &mut selected, &mut ordered, max_objects)?;
                    stack.push(crate::AnnotatedTag::parse(object.data())?.target());
                }
                ObjectKind::Tree => wanted_trees.push(id),
                ObjectKind::Blob => {
                    insert_upload_object(id, &mut selected, &mut ordered, max_objects)?;
                }
            }
        }

        self.collect_changed_trees(
            wanted_trees,
            boundary_trees,
            max_size,
            max_objects,
            &mut selected,
            &mut ordered,
        )?;
        Ok(ordered)
    }

    fn collect_changed_trees(
        &self,
        wanted_roots: Vec<ObjectId>,
        have_roots: Vec<ObjectId>,
        max_size: usize,
        max_objects: usize,
        selected: &mut HashSet<ObjectId>,
        ordered: &mut Vec<ObjectId>,
    ) -> Result<()> {
        let mut stack = vec![(wanted_roots, have_roots)];
        while let Some((wanted_ids, have_ids)) = stack.pop() {
            let have_ids = have_ids.into_iter().collect::<HashSet<_>>();
            let wanted_ids = wanted_ids
                .into_iter()
                .filter(|id| !have_ids.contains(id))
                .collect::<BTreeSet<_>>();
            if wanted_ids.is_empty() {
                continue;
            }

            let mut entries = BTreeMap::<Vec<u8>, (Vec<_>, Vec<_>)>::new();
            for id in wanted_ids {
                if selected.insert(id) {
                    ordered.push(id);
                    ensure_object_limit(selected.len(), max_objects)?;
                }
                let object = self.read_object_for_upload(id, max_size)?;
                if object.kind() != ObjectKind::Tree {
                    return Err(Error::InvalidTree(format!("object {id} is not a tree")));
                }
                for entry in crate::Tree::parse(object.data())?.entries() {
                    entries
                        .entry(entry.name().to_vec())
                        .or_default()
                        .0
                        .push(entry.clone());
                }
            }
            for id in have_ids {
                let object = self.read_object_for_upload(id, max_size)?;
                if object.kind() != ObjectKind::Tree {
                    return Err(Error::InvalidTree(format!("object {id} is not a tree")));
                }
                for entry in crate::Tree::parse(object.data())?.entries() {
                    entries
                        .entry(entry.name().to_vec())
                        .or_default()
                        .1
                        .push(entry.clone());
                }
            }
            let have_objects = entries
                .values()
                .flat_map(|(_, have)| have.iter().map(crate::TreeEntry::id))
                .collect::<HashSet<_>>();

            for (_, (wanted, have)) in entries.into_iter().rev() {
                let wanted_subtrees = wanted
                    .iter()
                    .filter(|entry| entry.mode() == EntryMode::Tree)
                    .map(crate::TreeEntry::id)
                    .collect::<Vec<_>>();
                if !wanted_subtrees.is_empty() {
                    let have_subtrees = have
                        .iter()
                        .filter(|entry| entry.mode() == EntryMode::Tree)
                        .map(crate::TreeEntry::id)
                        .collect();
                    stack.push((wanted_subtrees, have_subtrees));
                }
                for entry in wanted {
                    if entry.mode() != EntryMode::Tree
                        && entry.mode() != EntryMode::Gitlink
                        && !have_objects.contains(&entry.id())
                    {
                        insert_upload_object(entry.id(), selected, ordered, max_objects)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn reachable_objects_excluding(
        &self,
        roots: &[ObjectId],
        max_size: usize,
        ignore_missing_roots: bool,
        max_objects: usize,
        shallow: &BTreeSet<ObjectId>,
        excluded: &HashSet<ObjectId>,
    ) -> Result<Vec<ObjectId>> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        let mut stack = roots.iter().rev().copied().collect::<Vec<_>>();
        while let Some(id) = stack.pop() {
            if excluded.contains(&id) || seen.contains(&id) {
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
                    let (tree, parents) = crate::commit::parse_commit_links(object.data())?;
                    if !shallow.contains(&id) {
                        for parent in parents.iter().rev() {
                            stack.push(*parent);
                        }
                    }
                    stack.push(tree);
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

    pub(crate) fn reachable_objects_at_depth(
        &self,
        roots: &[ObjectId],
        depth: usize,
        max_size: usize,
        max_objects: usize,
    ) -> Result<(Vec<ObjectId>, BTreeSet<ObjectId>)> {
        if depth == 0 {
            return protocol_error("shallow depth must be positive");
        }
        let mut objects = BTreeSet::new();
        let mut ordered = Vec::new();
        let mut boundaries = BTreeSet::new();
        let mut commits = std::collections::VecDeque::new();
        for root in roots {
            let mut id = *root;
            loop {
                let object = self.read_object(id, max_size)?;
                if objects.insert(id) {
                    ordered.push(id);
                    ensure_object_limit(objects.len(), max_objects)?;
                }
                if object.kind() != ObjectKind::Tag {
                    if object.kind() == ObjectKind::Commit {
                        commits.push_back((id, 1usize));
                    } else {
                        self.collect_non_commit_closure(
                            id,
                            max_size,
                            max_objects,
                            &mut objects,
                            &mut ordered,
                        )?;
                    }
                    break;
                }
                id = crate::AnnotatedTag::parse(object.data())?.target();
            }
        }
        let mut commit_depths = std::collections::BTreeMap::new();
        let repository_shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: max_objects,
            max_object_size: max_size,
        })?;
        while let Some((id, current_depth)) = commits.pop_front() {
            if commit_depths
                .get(&id)
                .is_some_and(|known| *known <= current_depth)
            {
                continue;
            }
            commit_depths.insert(id, current_depth);
            let commit = self.read_commit(id, max_size)?;
            if objects.insert(id) {
                ordered.push(id);
                ensure_object_limit(objects.len(), max_objects)?;
            }
            self.collect_non_commit_closure(
                commit.tree(),
                max_size,
                max_objects,
                &mut objects,
                &mut ordered,
            )?;
            if repository_shallow.contains(&id) {
                boundaries.insert(id);
            } else if current_depth == depth {
                if !commit.parents().is_empty() {
                    boundaries.insert(id);
                }
            } else {
                for parent in commit.parents() {
                    commits.push_back((*parent, current_depth.saturating_add(1)));
                }
            }
        }
        Ok((ordered, boundaries))
    }

    pub(crate) fn reachable_objects_deepen_relative(
        &self,
        roots: &[ObjectId],
        client_shallow: &BTreeSet<ObjectId>,
        additional_depth: usize,
        max_size: usize,
        max_objects: usize,
    ) -> Result<(Vec<ObjectId>, BTreeSet<ObjectId>, BTreeSet<ObjectId>)> {
        if additional_depth == 0 {
            return protocol_error("relative deepen depth must be positive");
        }
        let mut ordered = self.reachable_objects_stopping_at(
            roots,
            max_size,
            false,
            max_objects,
            client_shallow,
        )?;
        let mut objects = ordered.iter().copied().collect::<BTreeSet<_>>();
        let reached = client_shallow
            .iter()
            .copied()
            .filter(|id| objects.contains(id))
            .collect::<BTreeSet<_>>();
        let repository_shallow = self.shallow_commits(&crate::ShallowOptions {
            max_commits: max_objects,
            max_object_size: max_size,
        })?;
        let mut boundaries = BTreeSet::new();
        let mut unshallow = BTreeSet::new();
        let mut commits = std::collections::VecDeque::new();
        for id in reached {
            if repository_shallow.contains(&id) {
                boundaries.insert(id);
                continue;
            }
            unshallow.insert(id);
            for parent in self.read_commit(id, max_size)?.parents() {
                commits.push_back((*parent, 1usize));
            }
        }
        let mut commit_depths = std::collections::BTreeMap::new();
        while let Some((id, current_depth)) = commits.pop_front() {
            if commit_depths
                .get(&id)
                .is_some_and(|known| *known <= current_depth)
            {
                continue;
            }
            commit_depths.insert(id, current_depth);
            let commit = self.read_commit(id, max_size)?;
            if objects.insert(id) {
                ordered.push(id);
                ensure_object_limit(objects.len(), max_objects)?;
            }
            self.collect_non_commit_closure(
                commit.tree(),
                max_size,
                max_objects,
                &mut objects,
                &mut ordered,
            )?;
            if repository_shallow.contains(&id)
                || current_depth == additional_depth && !commit.parents().is_empty()
            {
                boundaries.insert(id);
            } else {
                for parent in commit.parents() {
                    commits.push_back((*parent, current_depth.saturating_add(1)));
                }
            }
        }
        Ok((ordered, boundaries, unshallow))
    }

    fn collect_non_commit_closure(
        &self,
        root: ObjectId,
        max_size: usize,
        max_objects: usize,
        objects: &mut BTreeSet<ObjectId>,
        ordered: &mut Vec<ObjectId>,
    ) -> Result<()> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !objects.insert(id) {
                continue;
            }
            ordered.push(id);
            ensure_object_limit(objects.len(), max_objects)?;
            let object = self.read_object(id, max_size)?;
            match object.kind() {
                ObjectKind::Tree => {
                    let tree = crate::Tree::parse(object.data())?;
                    for entry in tree.entries().iter().rev() {
                        if entry.mode() != EntryMode::Gitlink {
                            stack.push(entry.id());
                        }
                    }
                }
                ObjectKind::Blob => {}
                ObjectKind::Tag | ObjectKind::Commit => {
                    return protocol_error("non-commit closure reached commit or tag");
                }
            }
        }
        Ok(())
    }
}

fn ensure_object_limit(count: usize, max_objects: usize) -> Result<()> {
    if count > max_objects {
        return Err(Error::InvalidObject(
            "reachable object traversal exceeds limit".into(),
        ));
    }
    Ok(())
}

fn insert_upload_object(
    id: ObjectId,
    selected: &mut HashSet<ObjectId>,
    ordered: &mut Vec<ObjectId>,
    max_objects: usize,
) -> Result<()> {
    if selected.insert(id) {
        ordered.push(id);
        ensure_object_limit(selected.len(), max_objects)?;
    }
    Ok(())
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
            "side-band-64k" | "thin-pack" | "ofs-delta" | "no-progress" | "shallow"
            | "deepen-relative" => capability.value().is_none(),
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
    use std::collections::BTreeSet;

    use super::{UploadPackOptions, UploadPackRequest};
    use crate::object::sha1;
    use crate::{
        CommitBuilder, CommitGraphOptions, EntryMode, InitOptions, MemoryFileSystem, ObjectId,
        ObjectKind, PktLine, PktLineDecoder, PreviousValue, ReferenceName, Repository, Sideband,
        Signature, Tree, TreeEntry,
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

        let thin = request_bytes(&[
            PktLine::Data(format!("want {id} thin-pack\n").into_bytes()),
            PktLine::Flush,
            PktLine::Data(b"done\n".to_vec()),
        ]);
        assert!(UploadPackRequest::parse(&thin).is_ok());

        let invalid = request_bytes(&[
            PktLine::Data(format!("want {id} unsupported\n").into_bytes()),
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
    fn commit_graph_reachability_uses_covered_parents_and_stops_at_shallows() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], 1);
        let middle = commit(&repository, &[root], 2);
        let tip = commit(&repository, &[middle], 3);
        repository
            .write_commit_graph(&[tip], &CommitGraphOptions::default())
            .unwrap();
        remove_loose_object(&repository, middle);

        let common = repository
            .reachable_commits(&[tip], 4096, 10, &BTreeSet::default())
            .unwrap();
        assert_eq!(common.commits, [tip, middle, root].into_iter().collect());

        let shallow = [middle].into_iter().collect();
        let common = repository
            .reachable_commits(&[tip], 4096, 10, &shallow)
            .unwrap();
        assert_eq!(common.commits, [tip, middle].into_iter().collect());
    }

    #[test]
    fn commit_graph_reachability_ignores_replacements_for_uploads() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let root = commit(&repository, &[], 1);
        let tip = commit(&repository, &[root], 2);
        repository
            .write_commit_graph(&[tip], &CommitGraphOptions::default())
            .unwrap();
        remove_loose_object(&repository, tip);
        assert!(
            repository
                .reachable_commits(&[tip], 4096, 10, &BTreeSet::default())
                .unwrap()
                .commits
                .is_empty()
        );

        let replacement_repository = Repository::init(
            MemoryFileSystem::new(),
            "replacement",
            &InitOptions::default(),
        )
        .unwrap();
        let root = commit(&replacement_repository, &[], 1);
        let tip = commit(&replacement_repository, &[root], 2);
        replacement_repository
            .write_commit_graph(&[tip], &CommitGraphOptions::default())
            .unwrap();
        let unrelated = commit(&replacement_repository, &[], 3);
        replacement_repository
            .create_replacement(tip, unrelated, false, 4096)
            .unwrap();
        let common = replacement_repository
            .reachable_commits(&[tip], 4096, 10, &BTreeSet::default())
            .unwrap();
        assert_eq!(common.commits, [tip, root].into_iter().collect());
    }

    #[test]
    fn tree_aware_selection_prunes_unchanged_trees_without_reading_blobs() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let identity = Signature::new("A", "a@example.com", 1, 0).unwrap();
        let unchanged_blob = repository
            .write_object(ObjectKind::Blob, &[b'x'; 4096])
            .unwrap();
        let changed_blob = repository
            .write_object(ObjectKind::Blob, b"changed")
            .unwrap();
        let unchanged_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"large".to_vec(), unchanged_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let old_changed_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), unchanged_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let base_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"stable".to_vec(), unchanged_tree).unwrap(),
                    TreeEntry::new(EntryMode::Tree, b"work".to_vec(), old_changed_tree).unwrap(),
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
        let changed_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"file".to_vec(), changed_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let tip_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, b"stable".to_vec(), unchanged_tree).unwrap(),
                    TreeEntry::new(EntryMode::Tree, b"work".to_vec(), changed_tree).unwrap(),
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

        let common = repository
            .reachable_commits(&[base_commit], 1024, 100, &BTreeSet::new())
            .unwrap();
        let selected = repository
            .select_upload_objects(&[tip_commit], &common, 1024, 100)
            .unwrap();

        assert_eq!(
            selected.into_iter().collect::<BTreeSet<_>>(),
            [tip_commit, tip_tree, changed_tree, changed_blob]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn tree_aware_selection_conservatively_resends_cross_path_objects() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let identity = Signature::new("A", "a@example.com", 1, 0).unwrap();
        let reused_blob = repository
            .write_object(ObjectKind::Blob, b"reused")
            .unwrap();
        let old_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"old-name".to_vec(), reused_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let old_commit = repository
            .write_commit(&CommitBuilder::new(old_tree, identity.clone(), identity.clone()).build())
            .unwrap();
        let empty_tree = repository.write_object(ObjectKind::Tree, b"").unwrap();
        let have = repository
            .write_commit(
                &CommitBuilder::new(empty_tree, identity.clone(), identity.clone())
                    .parent(old_commit)
                    .build(),
            )
            .unwrap();
        let wanted_tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b"new-name".to_vec(), reused_blob).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let want = repository
            .write_commit(
                &CommitBuilder::new(wanted_tree, identity.clone(), identity)
                    .parent(have)
                    .build(),
            )
            .unwrap();

        let common = repository
            .reachable_commits(&[have], 1024, 100, &BTreeSet::new())
            .unwrap();
        let selected = repository
            .select_upload_objects(&[want], &common, 1024, 100)
            .unwrap();

        assert_eq!(
            selected.into_iter().collect::<BTreeSet<_>>(),
            [want, wanted_tree, reused_blob].into_iter().collect()
        );
    }

    #[test]
    fn tree_aware_selection_preserves_multiple_wants_and_haves() {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let have_one = commit(&repository, &[], 1);
        let have_two = commit(&repository, &[], 2);
        let want_one = commit(&repository, &[have_one], 3);
        let want_two = commit(&repository, &[have_two], 4);

        let common = repository
            .reachable_commits(&[have_one, have_two], 1024, 100, &BTreeSet::new())
            .unwrap();
        let selected = repository
            .select_upload_objects(&[want_one, want_two], &common, 1024, 100)
            .unwrap();

        assert_eq!(
            selected.into_iter().collect::<BTreeSet<_>>(),
            [want_one, want_two].into_iter().collect()
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

    fn commit(repository: &Repository, parents: &[ObjectId], timestamp: i64) -> ObjectId {
        let tree = repository.write_object(ObjectKind::Tree, b"").unwrap();
        let signature = Signature::new("A", "a@example.com", timestamp, 0).unwrap();
        let mut builder = CommitBuilder::new(tree, signature.clone(), signature);
        for parent in parents {
            builder = builder.parent(*parent);
        }
        repository.write_commit(&builder.build()).unwrap()
    }

    fn remove_loose_object(repository: &Repository, id: ObjectId) {
        let hex = id.to_string();
        repository
            .filesystem()
            .remove_file(&repository.git_path(format!("objects/{}/{}", &hex[..2], &hex[2..])))
            .unwrap();
    }

    fn request_bytes(packets: &[PktLine]) -> Vec<u8> {
        packets
            .iter()
            .flat_map(|packet| packet.encode().unwrap())
            .collect()
    }
}
