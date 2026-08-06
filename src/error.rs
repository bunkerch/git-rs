use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidPath(PathBuf),
    AlreadyExists(PathBuf),
    NotFound(PathBuf),
    NotDirectory(PathBuf),
    IsDirectory(PathBuf),
    InvalidObjectId(String),
    InvalidObject(String),
    ObjectTooLarge { declared: u64, limit: usize },
    Compression(String),
    InvalidTree(String),
    InvalidCommit(String),
    InvalidReferenceName(String),
    InvalidReference(String),
    ReferenceConflict(String),
    SymbolicReferenceLoop(String),
    InvalidRepository(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidPath(path) => write!(f, "invalid repository path: {}", path.display()),
            Self::AlreadyExists(path) => write!(f, "path already exists: {}", path.display()),
            Self::NotFound(path) => write!(f, "path not found: {}", path.display()),
            Self::NotDirectory(path) => write!(f, "not a directory: {}", path.display()),
            Self::IsDirectory(path) => write!(f, "is a directory: {}", path.display()),
            Self::InvalidObjectId(value) => write!(f, "invalid object ID: {value}"),
            Self::InvalidObject(message) => write!(f, "invalid Git object: {message}"),
            Self::ObjectTooLarge { declared, limit } => {
                write!(
                    f,
                    "object declares {declared} bytes, exceeding limit {limit}"
                )
            }
            Self::Compression(message) => write!(f, "zlib error: {message}"),
            Self::InvalidTree(message) => write!(f, "invalid tree: {message}"),
            Self::InvalidCommit(message) => write!(f, "invalid commit: {message}"),
            Self::InvalidReferenceName(name) => write!(f, "invalid reference name: {name}"),
            Self::InvalidReference(message) => write!(f, "invalid reference: {message}"),
            Self::ReferenceConflict(name) => {
                write!(f, "reference changed concurrently: {name}")
            }
            Self::SymbolicReferenceLoop(name) => {
                write!(
                    f,
                    "symbolic reference depth exceeded while resolving {name}"
                )
            }
            Self::InvalidRepository(message) => write!(f, "invalid repository: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
