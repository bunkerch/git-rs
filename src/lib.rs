#![doc = include_str!("../README.md")]

pub mod commit;
pub mod error;
pub mod fs;
pub mod index;
pub mod object;
pub mod operations;
pub mod pack;
pub mod protocol;
pub mod receive_pack;
pub mod refs;
pub mod repository;
pub mod status;
pub mod tree;
pub mod upload_pack;
pub mod worktree;

pub use commit::{Commit, CommitBuilder, ExtraHeader, Signature};
pub use error::{Error, Result};
pub use fs::{FileStat, FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use index::{Index, IndexEntry, IndexExtension, IndexVersion, StatData};
pub use object::{Object, ObjectId, ObjectKind};
pub use operations::{ResetMode, ResetOptions, SwitchOptions};
pub use pack::{
    IncomingPackOptions, PackBundle, PackIndex, PackIndexEntry, PackOptions, ValidatedPack,
    WrittenPack,
};
pub use protocol::{Capability, PktLine, PktLineDecoder, Sideband};
pub use receive_pack::{
    ReceiveCommand, ReceiveCommandStatus, ReceivePackOptions, ReceivePackRequest, ReceivePackResult,
};
pub use refs::{PreviousValue, Reference, ReferenceName, ReferenceTarget, ReflogEntry};
pub use repository::{InitOptions, Repository};
pub use status::{ChangeKind, RepositoryStatus, StatusEntry, StatusOptions};
pub use tree::{EntryMode, Tree, TreeEntry};
pub use upload_pack::{UploadPackOptions, UploadPackRequest};
pub use worktree::CheckoutOptions;
