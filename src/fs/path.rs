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
/// (see [`reject_backslash`]), and empty/`.`/`..`/`.git` components, matching
/// git's cross-platform strictness. Known residual: a Windows drive-prefix
/// component such as `C:` is not rejected here, because rejecting it would
/// also break legitimate Unix filenames containing a colon.
pub(crate) fn validate_path(path: &[u8]) -> Result<()> {
    reject_backslash(path)?;
    if path.is_empty()
        || path[0] == b'/'
        || path.contains(&0)
        || path.split(|byte| *byte == b'/').any(|part| {
            part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git")
        })
    {
        return Err(Error::InvalidRepository("unsafe repository path".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{reject_backslash, validate, validate_path};
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
