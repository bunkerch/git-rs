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

/// Reject a Windows backslash separator in a repository-internal path.
///
/// Git treats backslashes as path separators on every platform
/// (`is_xplatform_dir_sep` in git-compat-util.h), and Windows resolves them
/// as separators in `PathBuf`, so a backslash that survives into a worktree
/// path can escape the worktree. git-rs's index already forbids backslashes
/// on every platform (see [`validate_path`]), so rejecting them here too is
/// consistent and causes no capability regression on Unix, where a literal
/// backslash would otherwise be a legal filename byte.
pub(crate) fn reject_backslash(path: &[u8]) -> Result<()> {
    if path.contains(&b'\\') {
        return Err(Error::InvalidRepository(
            "backslash in repository path".into(),
        ));
    }
    Ok(())
}

/// Validate a repository-internal path (an index or submodule path) before it
/// is materialized in the worktree.
///
/// Rejects empty and absolute paths, NUL bytes, Windows backslash separators
/// (see [`reject_backslash`]), and empty/`.`/`..`/`.git` components (including
/// the Windows-normalized `.git` aliases detected by [`is_ntfs_dotgit`]),
/// matching git's cross-platform strictness. Known residual: a Windows
/// drive-prefix component such as `C:` is not rejected here, because rejecting
/// it would also break legitimate Unix filenames containing a colon.
pub(crate) fn validate_path(path: &[u8]) -> Result<()> {
    reject_backslash(path)?;
    if path.is_empty()
        || path[0] == b'/'
        || path.contains(&0)
        || path.split(|byte| *byte == b'/').any(|part| {
            part.is_empty() || part == b"." || part == b".." || is_ntfs_dotgit(part)
        })
    {
        return Err(Error::InvalidRepository("unsafe repository path".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_ntfs_dotgit, reject_backslash, validate, validate_path};
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

    #[test]
    fn reject_backslash_rejects_windows_separators_on_all_platforms() {
        assert!(reject_backslash(b"..\\pwned").is_err());
        assert!(reject_backslash(b"foo\\bar").is_err());
        assert!(reject_backslash(b"a\\b\\c").is_err());
        assert!(reject_backslash(b"deps/lib").is_ok());
        assert!(reject_backslash(b"plain").is_ok());
    }

    #[test]
    fn validate_path_rejects_unsafe_repository_paths() {
        for path in [
            &b""[..],
            b"/absolute",
            b"..",
            b"a/../b",
            b"./a",
            b"a//b",
            b"a/b/",
            b".git",
            b"a/.git/b",
            b".GIT",
            b"a\0b",
            b"..\\pwned",
            b"foo\\bar",
        ] {
            assert!(
                validate_path(path).is_err(),
                "expected {path:?} to be rejected"
            );
        }
        for path in [&b"a/b"[..], b"a", b"deps/lib"] {
            assert!(
                validate_path(path).is_ok(),
                "expected {path:?} to be accepted"
            );
        }
    }
}
