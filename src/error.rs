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
    BeyondSymbolicLink(PathBuf),
    SymbolicReferenceLoop(String),
    InvalidRepository(String),
}

/// Replace control characters with `?` so attacker-controlled bytes in path or
/// message strings never reach terminal or log output verbatim.
///
/// The covered range is `0x00-0x1f` plus `0x7f` (DEL), and additionally the C1
/// control range `0x80-0x9f`, via `char::is_control`. Unlike git's `vfreportf`
/// sanitizer, which exempts `\t` and `\n`, this intentionally also replaces
/// `\t`, `\r`, and `\n`: checkout-conflict paths are rendered comma-joined on a
/// single line, so an embedded newline could otherwise be used to forge log
/// lines or hide surrounding text on a terminal.
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
            Self::Io(error) => write!(
                f,
                "I/O error: {}",
                sanitize_control_bytes(&error.to_string())
            ),
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
            Self::BeyondSymbolicLink(path) => {
                write!(f, "path is beyond a symbolic link: {}", sanitize_path(path))
            }
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
    use std::io;
    use std::path::PathBuf;

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

    #[test]
    fn path_variants_also_neutralize_control_bytes() {
        let evil = PathBuf::from("evil\x1b[31mname.txt");
        let cases: &[(&str, Error)] = &[
            (
                "invalid repository path: evil?[31mname.txt",
                Error::InvalidPath(evil.clone()),
            ),
            (
                "path already exists: evil?[31mname.txt",
                Error::AlreadyExists(evil.clone()),
            ),
            (
                "path not found: evil?[31mname.txt",
                Error::NotFound(evil.clone()),
            ),
            (
                "not a directory: evil?[31mname.txt",
                Error::NotDirectory(evil.clone()),
            ),
            (
                "directory is not empty: evil?[31mname.txt",
                Error::DirectoryNotEmpty(evil.clone()),
            ),
            (
                "is a directory: evil?[31mname.txt",
                Error::IsDirectory(evil.clone()),
            ),
            (
                "path is ignored: evil?[31mname.txt",
                Error::IgnoredPath(evil.clone()),
            ),
        ];
        for (expected, error) in cases {
            assert_eq!(error.to_string(), *expected);
        }
    }

    #[test]
    fn io_variant_also_neutralizes_control_bytes() {
        let inner = io::Error::other("evil\x1b[31mmessage");
        assert_eq!(Error::Io(inner).to_string(), "I/O error: evil?[31mmessage");
    }

    #[test]
    fn checkout_conflict_never_emits_raw_control_bytes() {
        for byte in 0x00..=0x1f_u8 {
            let c = char::from(byte);
            let rendered = Error::CheckoutConflict(vec![format!("a{c}b")]).to_string();
            assert!(
                !rendered.contains(c),
                "byte {byte:#x} leaked raw into {rendered:?}"
            );
            assert!(rendered.contains('?'), "byte {byte:#x} not replaced: {rendered:?}");
        }
        let c = '\x7f';
        let rendered = Error::CheckoutConflict(vec![format!("a{c}b")]).to_string();
        assert!(!rendered.contains(c), "byte 0x7f leaked raw into {rendered:?}");
        assert!(rendered.contains('?'), "byte 0x7f not replaced: {rendered:?}");
    }
}
