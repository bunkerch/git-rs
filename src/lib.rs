#![doc = include_str!("../README.md")]

pub mod am;
pub mod apply;
pub mod archive;
pub mod bisect;
pub mod blame;
pub mod branch;
pub mod bundle;
pub mod clean;
pub mod commit;
pub mod commit_graph;
pub mod commit_operation;
pub mod config;
pub mod count_objects;
pub mod cruft;
pub mod describe;
pub mod diff;
pub mod error;
pub mod fetch;
pub mod format_patch;
pub mod fs;
pub mod fsck;
pub mod gc;
pub mod grep;
pub mod ignore;
pub mod index;
pub mod linked_worktree;
pub mod log;
pub mod ls_files;
pub mod ls_tree;
pub mod maintenance;
pub mod merge;
pub mod multi_pack_index;
pub mod notes;
pub mod object;
pub mod operations;
pub mod pack;
pub mod pack_refs;
pub mod protocol;
pub mod prune;
pub mod pull;
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
pub mod server_info;
pub mod shallow;
pub mod show_ref;
pub mod sparse_checkout;
pub mod stash;
pub mod status;
pub mod submodule;
pub mod tag;
pub mod tree;
pub mod upload_pack;
pub mod upload_pack_v2;
pub mod worktree;

pub use am::{AmOptions, AmProgress, AmState};
pub use apply::{ApplyOptions, ApplyReport};
pub use archive::{ArchiveFormat, ArchiveOptions};
pub use bisect::{BisectMark, BisectOptions, BisectOutcome, BisectState};
pub use blame::{BlameLine, BlameOptions};
pub use branch::{DeleteBranchOptions, RenameBranchOptions};
pub use bundle::{
    BundleCreateOptions, BundleParseOptions, BundlePrerequisite, BundleReference, BundleVersion,
    GitBundle,
};
pub use clean::{CleanEntry, CleanIgnoredMode, CleanOptions};
pub use commit::{Commit, CommitBuilder, ExtraHeader, Signature};
pub use commit_graph::{CommitGraph, CommitGraphEntry, CommitGraphOptions, CommitGraphReport};
pub use commit_operation::CommitOptions;
pub use config::{Config, ConfigEntry};
pub use count_objects::{
    CountObjectsOptions, CountObjectsReport, ObjectGarbage, ObjectGarbageReason,
};
pub use cruft::CruftMtimes;
pub use describe::{DescribeOptions, Description};
pub use diff::{DiffEntry, DiffKind, DiffOptions};
pub use error::{Error, Result};
pub use fetch::{
    CloneOptions, FetchOptions, FetchResult, RemoteAdvertisement, RemoteRef, RepositoryTransport,
    UploadPackTransport,
};
pub use format_patch::{FormatPatch, FormatPatchNumbering, FormatPatchOptions};
pub use fs::{FileStat, FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use fsck::{FsckOptions, FsckReport};
pub use gc::{GcOptions, GcReport};
pub use grep::{GrepBinaryMode, GrepMatch, GrepOptions, GrepTarget};
pub use ignore::{IgnoreMatcher, IgnoreRule};
pub use index::{Index, IndexEntry, IndexExtension, IndexVersion, StatData};
pub use linked_worktree::{
    AddWorktreeOptions, LinkedWorktreeInfo, MoveWorktreeOptions, RemoveWorktreeOptions,
    WorktreePruneEntry, WorktreePruneOptions, WorktreePruneReason, WorktreeTarget,
};
pub use log::{LogEntry, LogOptions, LogParentDiff};
pub use ls_files::{LsFilesEntry, LsFilesKind, LsFilesOptions};
pub use ls_tree::{LsTreeEntry, LsTreeOptions};
pub use maintenance::{MaintenanceOptions, MaintenanceOutcome, MaintenanceReport, MaintenanceTask};
pub use merge::{
    FastForwardMode, MergeOptions, MergeResult, ReplayKind, ReplayOptions, ReplayResult,
};
pub use multi_pack_index::{
    MultiPackIndex, MultiPackIndexEntry, MultiPackIndexOptions, MultiPackIndexReport,
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
pub use pull::{PullIntegration, PullMode, PullOptions, PullResult};
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
    ReferenceTarget, ReflogEntry, ReflogRewriteOptions, ReflogRewriteResult,
};
pub use refspec::{RefSpec, RefSpecKind};
pub use remote::{Remote, RemoveRemoteResult};
pub use repack::{RepackOptions, RepackResult};
pub use replace::Replacement;
pub use repository::{InitOptions, Repository};
pub use restore::{RestoreOptions, RestoreTarget};
pub use revision::{GraphOptions, Revision, RevisionWalkOptions};
pub use revparse::{ResolvedObject, RevisionOptions};
pub use server_info::{ServerInfoOptions, ServerInfoReport};
pub use shallow::ShallowOptions;
pub use show_ref::{ShowRefEntry, ShowRefOptions};
pub use sparse_checkout::{SparseCheckoutOptions, SparseCheckoutReport, SparseCheckoutState};
pub use stash::{StashApplyOptions, StashApplyResult, StashEntry, StashPushOptions};
pub use status::{ChangeKind, RepositoryStatus, StatusEntry, StatusOptions};
pub use submodule::{
    Submodule, SubmoduleAddOptions, SubmoduleAddReport, SubmoduleDeinitOptions,
    SubmoduleDeinitReport, SubmoduleOptions, SubmoduleStatus, SubmoduleStatusKind,
    SubmoduleUpdateOptions, SubmoduleUpdateReport,
};
pub use tag::{AnnotatedTag, PeeledObject, TagBuilder};
pub use tree::{EntryMode, Tree, TreeEntry};
pub use upload_pack::{UploadPackOptions, UploadPackRequest};
pub use upload_pack_v2::{FetchV2Request, LsRefsRequest, UploadPackV2Limits, UploadPackV2Request};
pub use worktree::{AddOptions, CheckoutOptions, MoveOptions, RemoveOptions};
