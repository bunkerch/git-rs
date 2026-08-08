use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

pub(crate) fn validate(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvalidPath(path.to_path_buf()));
            }
        }
    }
    Ok(normalized)
}

/// Detect a single path component that Windows would resolve to `.git`.
///
/// This mirrors git's `is_ntfs_dotgit()` (path.c). Win32 strips trailing dots
/// and spaces from path components and resolves names case-insensitively, and
/// NTFS treats `<name>:<stream>` as an alternate data stream of `<name>`, so
/// `.git`, `.git.`, `.git `, `.Git.`, `.git::$INDEX_ALLOCATION`, and the NTFS
/// short name `git~1` all refer to the real `.git` directory. A component is
/// therefore dangerous when it starts with `.git` (case-insensitively) or the
/// short name `git~1`, followed by any run of `.`/` ` and then either a `:`
/// (alternate-data-stream terminator) or the end of the component.
pub(crate) fn is_ntfs_dotgit(component: &[u8]) -> bool {
    let lower = component.to_ascii_lowercase();
    let after = if let Some(rest) = lower.strip_prefix(b".git") {
        rest
    } else if let Some(rest) = lower.strip_prefix(b"git~1") {
        rest
    } else {
        return false;
    };
    for byte in after {
        if *byte == b'.' || *byte == b' ' {
            continue;
        }
        return *byte == b':';
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{is_ntfs_dotgit, validate};
    use std::path::Path;

    #[test]
    fn rejects_paths_which_escape_storage_root() {
        assert!(validate(Path::new("../outside")).is_err());
        assert!(validate(Path::new("/outside")).is_err());
        assert_eq!(
            validate(Path::new("objects/./pack")).unwrap(),
            Path::new("objects/pack")
        );
    }

    #[test]
    fn detects_win32_normalized_git_components() {
        for component in [
            b".git".as_slice(),
            b".git.".as_slice(),
            b".git ".as_slice(),
            b".Git.".as_slice(),
            b"git~1".as_slice(),
            b".git:$DATA".as_slice(),
            b".git::$INDEX_ALLOCATION".as_slice(),
            b".git:stream".as_slice(),
            b".git .:stream".as_slice(),
        ] {
            assert!(
                is_ntfs_dotgit(component),
                "expected {component:?} to be flagged as .git alias"
            );
        }
    }

    #[test]
    fn accepts_benign_components() {
        for component in [
            b".gitignore".as_slice(),
            b".gitmodules".as_slice(),
            b".gitattributes".as_slice(),
            b"foo.git".as_slice(),
            b"..git".as_slice(),
            b"git~1x".as_slice(),
            b"git~2".as_slice(),
            b"git~12".as_slice(),
            b".git_config".as_slice(),
        ] {
            assert!(
                !is_ntfs_dotgit(component),
                "expected {component:?} not to be flagged"
            );
        }
    }
}
