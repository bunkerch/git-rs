#![doc = include_str!("../README.md")]

pub mod archive;
pub mod branch;
pub mod bundle;
pub mod clean;
pub mod commit;
pub mod commit_operation;
pub mod config;
pub mod count_objects;
pub mod diff;
pub mod error;
pub mod fetch;
pub mod fs;
pub mod fsck;
pub mod ignore;
pub mod index;
pub mod linked_worktree;
pub mod ls_tree;
pub mod merge;
pub mod notes;
pub mod object;
pub mod operations;
pub mod pack;
pub mod pack_refs;
pub mod protocol;
pub mod prune;
pub mod push;
pub mod rebase;
pub mod receive_pack;
pub mod refs;
pub mod refspec;
pub mod remote;
pub mod repack;
pub mod replace;
pub mod repository;
pub mod restore;
pub mod revision;
pub mod revparse;
pub mod stash;
pub mod status;
pub mod tag;
pub mod tree;
pub mod upload_pack;
pub mod worktree;

pub use archive::{ArchiveFormat, ArchiveOptions};
pub use branch::{DeleteBranchOptions, RenameBranchOptions};
pub use bundle::{
    BundleCreateOptions, BundleParseOptions, BundlePrerequisite, BundleReference, BundleVersion,
    GitBundle,
};
pub use clean::{CleanEntry, CleanIgnoredMode, CleanOptions};
pub use commit::{Commit, CommitBuilder, ExtraHeader, Signature};
pub use commit_operation::CommitOptions;
pub use config::{Config, ConfigEntry};
pub use count_objects::{
    CountObjectsOptions, CountObjectsReport, ObjectGarbage, ObjectGarbageReason,
};
pub use diff::{DiffEntry, DiffKind, DiffOptions};
pub use error::{Error, Result};
pub use fetch::{
    CloneOptions, FetchOptions, FetchResult, RemoteAdvertisement, RemoteRef, RepositoryTransport,
    UploadPackTransport,
};
pub use fs::{FileStat, FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use fsck::{FsckOptions, FsckReport};
pub use ignore::{IgnoreMatcher, IgnoreRule};
pub use index::{Index, IndexEntry, IndexExtension, IndexVersion, StatData};
pub use linked_worktree::{AddWorktreeOptions, LinkedWorktreeInfo, WorktreeTarget};
pub use ls_tree::{LsTreeEntry, LsTreeOptions};
pub use merge::{
    FastForwardMode, MergeOptions, MergeResult, ReplayKind, ReplayOptions, ReplayResult,
};
pub use notes::{DEFAULT_NOTES_REF, Note, NotesOptions};
pub use object::{Object, ObjectId, ObjectKind};
pub use operations::{ResetMode, ResetOptions, SwitchOptions};
pub use pack::{
    IncomingPackOptions, PackBundle, PackIndex, PackIndexEntry, PackOptions, ValidatedPack,
    WrittenPack,
};
pub use pack_refs::{PackRefsOptions, PackRefsResult};
pub use protocol::{Capability, PktLine, PktLineDecoder, Sideband};
pub use prune::{PruneEntry, PruneOptions, PruneReason};
pub use push::{
    InProcessReceivePackTransport, PushOptions, PushResult, PushStatus, PushUpdate,
    ReceivePackTransport,
};
pub use rebase::{RebaseEmpty, RebaseOptions, RebaseResult};
pub use receive_pack::{
    ReceiveCommand, ReceiveCommandStatus, ReceivePackOptions, ReceivePackRequest, ReceivePackResult,
};
pub use refs::{
    PreviousReferenceValue, PreviousValue, Reference, ReferenceEdit, ReferenceName,
    ReferenceTarget, ReflogEntry,
};
pub use refspec::{RefSpec, RefSpecKind};
pub use remote::{Remote, RemoveRemoteResult};
pub use repack::{RepackOptions, RepackResult};
pub use replace::Replacement;
pub use repository::{InitOptions, Repository};
pub use restore::{RestoreOptions, RestoreTarget};
pub use revision::{GraphOptions, Revision, RevisionWalkOptions};
pub use revparse::{ResolvedObject, RevisionOptions};
pub use stash::{StashApplyOptions, StashApplyResult, StashEntry, StashPushOptions};
pub use status::{ChangeKind, RepositoryStatus, StatusEntry, StatusOptions};
pub use tag::{AnnotatedTag, PeeledObject, TagBuilder};
pub use tree::{EntryMode, Tree, TreeEntry};
pub use upload_pack::{UploadPackOptions, UploadPackRequest};
pub use worktree::{AddOptions, CheckoutOptions, MoveOptions, RemoveOptions};
