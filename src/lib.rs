#![doc = include_str!("../README.md")]

pub mod commit;
pub mod error;
pub mod fs;
pub mod index;
pub mod object;
pub mod refs;
pub mod repository;
pub mod status;
pub mod tree;
pub mod worktree;

pub use commit::{Commit, CommitBuilder, ExtraHeader, Signature};
pub use error::{Error, Result};
pub use fs::{FileStat, FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use index::{Index, IndexEntry, IndexExtension, IndexVersion, StatData};
pub use object::{Object, ObjectId, ObjectKind};
pub use refs::{PreviousValue, Reference, ReferenceName, ReferenceTarget};
pub use repository::{InitOptions, Repository};
pub use status::{ChangeKind, RepositoryStatus, StatusEntry, StatusOptions};
pub use tree::{EntryMode, Tree, TreeEntry};
pub use worktree::CheckoutOptions;
