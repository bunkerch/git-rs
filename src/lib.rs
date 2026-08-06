#![doc = include_str!("../README.md")]

pub mod commit;
pub mod config;
pub mod diff;
pub mod error;
pub mod fetch;
pub mod fs;
pub mod ignore;
pub mod index;
pub mod linked_worktree;
pub mod merge;
pub mod object;
pub mod operations;
pub mod pack;
pub mod protocol;
pub mod push;
pub mod rebase;
pub mod receive_pack;
pub mod refs;
pub mod repository;
pub mod revision;
pub mod revparse;
pub mod status;
pub mod tag;
pub mod tree;
pub mod upload_pack;
pub mod worktree;

pub use commit::{Commit, CommitBuilder, ExtraHeader, Signature};
pub use config::{Config, ConfigEntry};
pub use diff::{DiffEntry, DiffKind, DiffOptions};
pub use error::{Error, Result};
pub use fetch::{
    CloneOptions, FetchOptions, FetchResult, RemoteAdvertisement, RemoteRef, RepositoryTransport,
    UploadPackTransport,
};
pub use fs::{FileStat, FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use ignore::{IgnoreMatcher, IgnoreRule};
pub use index::{Index, IndexEntry, IndexExtension, IndexVersion, StatData};
pub use linked_worktree::{AddWorktreeOptions, LinkedWorktreeInfo, WorktreeTarget};
pub use merge::{
    FastForwardMode, MergeOptions, MergeResult, ReplayKind, ReplayOptions, ReplayResult,
};
pub use object::{Object, ObjectId, ObjectKind};
pub use operations::{ResetMode, ResetOptions, SwitchOptions};
pub use pack::{
    IncomingPackOptions, PackBundle, PackIndex, PackIndexEntry, PackOptions, ValidatedPack,
    WrittenPack,
};
pub use protocol::{Capability, PktLine, PktLineDecoder, Sideband};
pub use push::{
    InProcessReceivePackTransport, PushOptions, PushResult, PushStatus, PushUpdate,
    ReceivePackTransport,
};
pub use rebase::{RebaseEmpty, RebaseOptions, RebaseResult};
pub use receive_pack::{
    ReceiveCommand, ReceiveCommandStatus, ReceivePackOptions, ReceivePackRequest, ReceivePackResult,
};
pub use refs::{
    PreviousValue, Reference, ReferenceEdit, ReferenceName, ReferenceTarget, ReflogEntry,
};
pub use repository::{InitOptions, Repository};
pub use revision::{GraphOptions, Revision, RevisionWalkOptions};
pub use revparse::{ResolvedObject, RevisionOptions};
pub use status::{ChangeKind, RepositoryStatus, StatusEntry, StatusOptions};
pub use tag::{AnnotatedTag, PeeledObject, TagBuilder};
pub use tree::{EntryMode, Tree, TreeEntry};
pub use upload_pack::{UploadPackOptions, UploadPackRequest};
pub use worktree::{AddOptions, CheckoutOptions};
