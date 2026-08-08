use std::fmt;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidPath(PathBuf),
    AlreadyExists(PathBuf),
    NotFound(PathBuf),
    NotDirectory(PathBuf),
    DirectoryNotEmpty(PathBuf),
    IsDirectory(PathBuf),
    InvalidObjectId(String),
    InvalidObject(String),
    ObjectTooLarge { declared: u64, limit: usize },
    Compression(String),
    Protocol(String),
    InvalidTree(String),
    InvalidCommit(String),
    EmptyReplay,
    InvalidReferenceName(String),
    InvalidReference(String),
    InvalidRevision(String),
    AmbiguousRevision(String),
    ReferenceConflict(String),
    CheckoutConflict(Vec<String>),
    IgnoredPath(PathBuf),
    SymbolicReferenceLoop(String),
    InvalidRepository(String),
}

/// Replace control characters (0x00-0x1f and 0x7f, matching git's `iscntrl`
/// neutralization in `vfreportf`) with `?` so attacker-controlled bytes in
/// path or message strings never reach terminal or log output verbatim.
fn sanitize_control_bytes(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn sanitize_path(path: &Path) -> String {
    sanitize_control_bytes(&path.display().to_string())
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidPath(path) => {
                write!(f, "invalid repository path: {}", sanitize_path(path))
            }
            Self::AlreadyExists(path) => write!(f, "path already exists: {}", sanitize_path(path)),
            Self::NotFound(path) => write!(f, "path not found: {}", sanitize_path(path)),
            Self::NotDirectory(path) => write!(f, "not a directory: {}", sanitize_path(path)),
            Self::DirectoryNotEmpty(path) => {
                write!(f, "directory is not empty: {}", sanitize_path(path))
            }
            Self::IsDirectory(path) => write!(f, "is a directory: {}", sanitize_path(path)),
            Self::InvalidObjectId(value) => {
                write!(f, "invalid object ID: {}", sanitize_control_bytes(value))
            }
            Self::InvalidObject(message) => {
                write!(f, "invalid Git object: {}", sanitize_control_bytes(message))
            }
            Self::ObjectTooLarge { declared, limit } => {
                write!(
                    f,
                    "object declares {declared} bytes, exceeding limit {limit}"
                )
            }
            Self::Compression(message) => {
                write!(f, "zlib error: {}", sanitize_control_bytes(message))
            }
            Self::Protocol(message) => {
                write!(f, "Git protocol error: {}", sanitize_control_bytes(message))
            }
            Self::InvalidTree(message) => {
                write!(f, "invalid tree: {}", sanitize_control_bytes(message))
            }
            Self::InvalidCommit(message) => {
                write!(f, "invalid commit: {}", sanitize_control_bytes(message))
            }
            Self::EmptyReplay => write!(f, "replayed commit would be empty"),
            Self::InvalidReferenceName(name) => {
                write!(f, "invalid reference name: {}", sanitize_control_bytes(name))
            }
            Self::InvalidReference(message) => {
                write!(f, "invalid reference: {}", sanitize_control_bytes(message))
            }
            Self::InvalidRevision(message) => {
                write!(f, "invalid revision: {}", sanitize_control_bytes(message))
            }
            Self::AmbiguousRevision(value) => {
                write!(f, "ambiguous revision: {}", sanitize_control_bytes(value))
            }
            Self::ReferenceConflict(name) => write!(
                f,
                "reference changed concurrently: {}",
                sanitize_control_bytes(name)
            ),
            Self::CheckoutConflict(paths) => {
                let paths = paths
                    .iter()
                    .map(|path| sanitize_control_bytes(path))
                    .collect::<Vec<_>>();
                write!(
                    f,
                    "checkout would overwrite local changes: {}",
                    paths.join(", ")
                )
            }
            Self::IgnoredPath(path) => write!(f, "path is ignored: {}", sanitize_path(path)),
            Self::SymbolicReferenceLoop(name) => write!(
                f,
                "symbolic reference depth exceeded while resolving {}",
                sanitize_control_bytes(name)
            ),
            Self::InvalidRepository(message) => {
                write!(f, "invalid repository: {}", sanitize_control_bytes(message))
            }
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

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn checkout_conflict_replaces_esc_with_question_mark() {
        let error = Error::CheckoutConflict(vec![
            "evil\x1b[31mname.txt".to_string(),
            "plain-name.txt".to_string(),
        ]);
        let rendered = error.to_string();
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.contains("evil?[31mname.txt"));
        assert_eq!(
            rendered,
            "checkout would overwrite local changes: evil?[31mname.txt, plain-name.txt"
        );
    }

    #[test]
    fn checkout_conflict_replaces_all_control_bytes() {
        let error = Error::CheckoutConflict(vec!["a\tb\nc\x07d\x7fe".to_string()]);
        assert_eq!(
            error.to_string(),
            "checkout would overwrite local changes: a?b?c?d?e"
        );
    }

    #[test]
    fn checkout_conflict_preserves_normal_paths() {
        let error = Error::CheckoutConflict(vec![
            "normal/path file.txt".to_string(),
            "unicode-ünïcode.txt".to_string(),
        ]);
        assert_eq!(
            error.to_string(),
            "checkout would overwrite local changes: normal/path file.txt, unicode-ünïcode.txt"
        );
    }

    #[test]
    fn string_variants_also_neutralize_control_bytes() {
        let error = Error::InvalidRepository("repo\x1b[31mname".to_string());
        assert_eq!(error.to_string(), "invalid repository: repo?[31mname");
        let error = Error::InvalidTree("path\x07name".to_string());
        assert_eq!(error.to_string(), "invalid tree: path?name");
    }
}
