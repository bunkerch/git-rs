//! Transport-neutral remote reference discovery.

use crate::fetch::query_refs_v2;
use crate::ignore::wildmatch_ref;
use crate::{
    Error, ObjectId, RemoteAdvertisement, Result, UploadPackTransport, UploadPackV2Transport,
};

/// Remote ref selection, protocol arguments, and resource bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsRemoteOptions {
    pub selection: LsRemoteSelection,
    pub refs_only: bool,
    pub show_symrefs: bool,
    pub patterns: Vec<String>,
    pub server_options: Vec<String>,
    pub max_refs: usize,
    pub max_advertisement_size: usize,
    pub max_output_size: usize,
}

impl Default for LsRemoteOptions {
    fn default() -> Self {
        Self {
            selection: LsRemoteSelection::All,
            refs_only: false,
            show_symrefs: false,
            patterns: Vec::new(),
            server_options: Vec::new(),
            max_refs: 10_000_000,
            max_advertisement_size: 1024 * 1024 * 1024,
            max_output_size: 1024 * 1024 * 1024,
        }
    }
}

/// Ref namespaces requested from and selected from the remote.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LsRemoteSelection {
    #[default]
    All,
    Branches,
    Tags,
    BranchesAndTags,
}

/// One selected remote reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LsRemoteEntry {
    id: ObjectId,
    name: String,
    symref_target: Option<String>,
}

impl LsRemoteEntry {
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn symref_target(&self) -> Option<&str> {
        self.symref_target.as_deref()
    }
}

/// Structured and Git-formatted results from remote discovery.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LsRemoteResult {
    entries: Vec<LsRemoteEntry>,
    output: Vec<u8>,
}

impl LsRemoteResult {
    #[must_use]
    pub fn entries(&self) -> &[LsRemoteEntry] {
        &self.entries
    }

    /// Return native Git-compatible tab-separated output.
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }
}

/// Query a protocol-v0/v1 upload-pack transport without fetching objects.
///
/// # Errors
/// Returns an error for malformed/oversized advertisements or output limits.
pub fn ls_remote<T: UploadPackTransport>(
    transport: &mut T,
    options: &LsRemoteOptions,
) -> Result<LsRemoteResult> {
    if !options.server_options.is_empty() {
        return ls_error("server options require protocol v2");
    }
    let bytes = transport.advertise()?;
    if bytes.len() > options.max_advertisement_size {
        return ls_error("ls-remote advertisement exceeds limit");
    }
    let advertisement = RemoteAdvertisement::parse_with_limit(&bytes, options.max_refs)?;
    select(&advertisement, options)
}

/// Query a protocol-v2 upload-pack transport with ls-refs.
///
/// # Errors
/// Returns an error for malformed/oversized capabilities, requests, responses,
/// server options, or output limits.
pub fn ls_remote_v2<T: UploadPackV2Transport>(
    transport: &mut T,
    options: &LsRemoteOptions,
) -> Result<LsRemoteResult> {
    let prefixes = match options.selection {
        LsRemoteSelection::All => Vec::new(),
        LsRemoteSelection::Branches => vec!["refs/heads/".to_owned()],
        LsRemoteSelection::Tags => vec!["refs/tags/".to_owned()],
        LsRemoteSelection::BranchesAndTags => {
            vec!["refs/heads/".to_owned(), "refs/tags/".to_owned()]
        }
    };
    let advertisement = query_refs_v2(
        transport,
        &prefixes,
        &options.server_options,
        options.max_refs,
        options.max_advertisement_size,
    )?;
    select(&advertisement, options)
}

fn select(
    advertisement: &RemoteAdvertisement,
    options: &LsRemoteOptions,
) -> Result<LsRemoteResult> {
    let mut entries = advertisement
        .refs()
        .iter()
        .filter(|reference| selected_type(reference.name(), options))
        .filter(|reference| matches_patterns(reference.name(), &options.patterns))
        .map(|reference| LsRemoteEntry {
            id: reference.id(),
            name: reference.name().to_owned(),
            symref_target: (options.show_symrefs && reference.name() == "HEAD")
                .then(|| advertisement.head_target().map(str::to_owned))
                .flatten(),
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    if entries.len() > options.max_refs {
        return ls_error("selected remote ref count exceeds limit");
    }
    let mut output = Vec::new();
    for entry in &entries {
        if let Some(target) = &entry.symref_target {
            append_bounded(
                &mut output,
                format!("ref: {target}\t{}\n", entry.name).as_bytes(),
                options.max_output_size,
            )?;
        }
        append_bounded(
            &mut output,
            format!("{}\t{}\n", entry.id, entry.name).as_bytes(),
            options.max_output_size,
        )?;
    }
    Ok(LsRemoteResult { entries, output })
}

fn selected_type(name: &str, options: &LsRemoteOptions) -> bool {
    if options.refs_only && (name == "HEAD" || name.ends_with("^{}")) {
        return false;
    }
    match options.selection {
        LsRemoteSelection::All => true,
        LsRemoteSelection::Branches => name.starts_with("refs/heads/"),
        LsRemoteSelection::Tags => name.starts_with("refs/tags/"),
        LsRemoteSelection::BranchesAndTags => {
            name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
        }
    }
}

fn matches_patterns(name: &str, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            suffixes(name).any(|suffix| wildmatch_ref(pattern.as_bytes(), suffix.as_bytes()))
        })
}

fn suffixes(name: &str) -> impl Iterator<Item = &str> {
    std::iter::once(name).chain(
        name.match_indices('/')
            .map(|(index, _)| &name[index.saturating_add(1)..]),
    )
}

fn append_bounded(output: &mut Vec<u8>, value: &[u8], limit: usize) -> Result<()> {
    let size = output
        .len()
        .checked_add(value.len())
        .ok_or_else(|| Error::Protocol("ls-remote output overflow".into()))?;
    if size > limit {
        return ls_error("ls-remote output exceeds limit");
    }
    output.extend_from_slice(value);
    Ok(())
}

fn ls_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{LsRemoteOptions, LsRemoteSelection, ls_remote, ls_remote_v2};
    use crate::{
        FastImportOptions, InitOptions, MemoryFileSystem, PktLine, Repository, RepositoryTransport,
        RepositoryV2Transport, UploadPackOptions, UploadPackV2Limits, UploadPackV2Transport,
    };

    #[test]
    fn v0_and_v2_list_identical_refs_symrefs_peels_and_tail_patterns() {
        let repository = repository();
        let mut v0 = RepositoryTransport::new(&repository, UploadPackOptions::default());
        let mut v2 = RepositoryV2Transport::new(
            &repository,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        let options = LsRemoteOptions {
            show_symrefs: true,
            ..LsRemoteOptions::default()
        };
        let one = ls_remote(&mut v0, &options).unwrap();
        let two = ls_remote_v2(&mut v2, &options).unwrap();
        assert_eq!(one.output(), two.output());
        assert!(String::from_utf8_lossy(one.output()).starts_with("ref: refs/heads/main\tHEAD\n"));
        assert!(
            one.entries()
                .iter()
                .any(|entry| entry.name() == "refs/tags/v1^{}")
        );

        let mut v0 = RepositoryTransport::new(&repository, UploadPackOptions::default());
        let matched = ls_remote(
            &mut v0,
            &LsRemoteOptions {
                selection: LsRemoteSelection::Branches,
                patterns: vec!["x".to_owned()],
                ..LsRemoteOptions::default()
            },
        )
        .unwrap();
        assert_eq!(matched.entries().len(), 1);
        assert_eq!(matched.entries()[0].name(), "refs/heads/topic/x");

        let mut v2 = RepositoryV2Transport::new(
            &repository,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        let tags = ls_remote_v2(
            &mut v2,
            &LsRemoteOptions {
                selection: LsRemoteSelection::Tags,
                refs_only: true,
                ..LsRemoteOptions::default()
            },
        )
        .unwrap();
        assert_eq!(tags.entries().len(), 1);
        assert_eq!(tags.entries()[0].name(), "refs/tags/v1");
    }

    #[test]
    fn enforces_advertisement_ref_output_and_server_option_limits() {
        let repository = repository();
        let mut v0 = RepositoryTransport::new(&repository, UploadPackOptions::default());
        assert!(
            ls_remote(
                &mut v0,
                &LsRemoteOptions {
                    max_refs: 1,
                    ..LsRemoteOptions::default()
                }
            )
            .is_err()
        );
        let mut v0 = RepositoryTransport::new(&repository, UploadPackOptions::default());
        assert!(
            ls_remote(
                &mut v0,
                &LsRemoteOptions {
                    max_output_size: 1,
                    ..LsRemoteOptions::default()
                }
            )
            .is_err()
        );
        let mut v0 = RepositoryTransport::new(&repository, UploadPackOptions::default());
        assert!(
            ls_remote(
                &mut v0,
                &LsRemoteOptions {
                    server_options: vec!["trace=1".to_owned()],
                    ..LsRemoteOptions::default()
                }
            )
            .is_err()
        );
        let mut v2 = RepositoryV2Transport::new(
            &repository,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        assert!(
            ls_remote_v2(
                &mut v2,
                &LsRemoteOptions {
                    server_options: vec!["bad\noption".to_owned()],
                    ..LsRemoteOptions::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn v2_reference_query_does_not_require_fetch_capability() {
        let repository = repository();
        let inner = RepositoryV2Transport::new(
            &repository,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        let mut transport = RefOnlyTransport { inner };
        let result = ls_remote_v2(&mut transport, &LsRemoteOptions::default()).unwrap();
        assert!(
            result
                .entries()
                .iter()
                .any(|entry| entry.name() == "refs/heads/main")
        );
    }

    struct RefOnlyTransport<'a> {
        inner: RepositoryV2Transport<'a>,
    }

    impl UploadPackV2Transport for RefOnlyTransport<'_> {
        fn advertise_v2(&mut self) -> crate::Result<Vec<u8>> {
            let mut output = Vec::new();
            for line in [
                b"version 2\n".as_slice(),
                b"ls-refs=unborn\n",
                b"object-format=sha1\n",
            ] {
                output.extend(PktLine::Data(line.to_vec()).encode()?);
            }
            output.extend(PktLine::Flush.encode()?);
            Ok(output)
        }

        fn request_v2(&mut self, request: &[u8]) -> crate::Result<Vec<u8>> {
            self.inner.request_v2(request)
        }
    }

    fn repository() -> Repository {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            ".",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        repository
            .fast_import(
                b"commit refs/heads/main\nmark :1\ncommitter A <a@b> 1 +0000\ndata 4\nmain\n\
commit refs/heads/topic/x\nmark :2\ncommitter A <a@b> 2 +0000\ndata 5\ntopic\nfrom :1\n\
tag v1\nmark :3\nfrom :1\ntagger T <t@b> 3 +0000\ndata 3\ntag\ndone\n",
                &FastImportOptions::default(),
            )
            .unwrap();
        repository
    }
}
