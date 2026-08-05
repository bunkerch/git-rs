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

#[cfg(test)]
mod tests {
    use super::validate;
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
}
