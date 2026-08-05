#![doc = include_str!("../README.md")]

pub mod error;
pub mod fs;
pub mod object;
pub mod refs;
pub mod repository;

pub use error::{Error, Result};
pub use fs::{FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use object::{Object, ObjectId, ObjectKind};
pub use refs::{PreviousValue, Reference, ReferenceName, ReferenceTarget};
pub use repository::{InitOptions, Repository};
