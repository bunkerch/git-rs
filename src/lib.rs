#![doc = include_str!("../README.md")]

pub mod error;
pub mod fs;
pub mod repository;

pub use error::{Error, Result};
pub use fs::{FileSystem, HostFileSystem, MemoryFileSystem, Metadata};
pub use repository::{InitOptions, Repository};
