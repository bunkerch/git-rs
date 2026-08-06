//! Transparent Git object replacement through `refs/replace/`.

use std::collections::BTreeMap;
use std::str::FromStr;

use crate::{Error, ObjectId, PreviousValue, ReferenceName, ReferenceTarget, Repository, Result};

const REPLACE_PREFIX: &str = "refs/replace/";
const MAX_REPLACE_DEPTH: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Replacement {
    original: ObjectId,
    replacement: ObjectId,
}

impl Replacement {
    #[must_use]
    pub const fn original(self) -> ObjectId {
        self.original
    }

    #[must_use]
    pub const fn replacement(self) -> ObjectId {
        self.replacement
    }
}

impl Repository {
    /// Resolve a possibly recursive replace ref, with Git's five-hop bound.
    ///
    /// `core.useReplaceRefs=false` returns the input unchanged.
    ///
    /// # Errors
    /// Returns an error for malformed configuration, references, replacement
    /// cycles/depth overflow, poisoned cache state, or storage failures.
    pub fn resolve_replacement(&self, id: ObjectId) -> Result<ObjectId> {
        self.prepare_replacements()?;
        let replacements = self
            .replacements
            .read()
            .map_err(|_| Error::InvalidRepository("replace cache lock poisoned".into()))?;
        let replacements = replacements
            .as_ref()
            .ok_or_else(|| Error::InvalidRepository("replace cache was not initialized".into()))?;
        let mut current = id;
        for _ in 0..MAX_REPLACE_DEPTH {
            match replacements.get(&current) {
                Some(next) => current = *next,
                None => return Ok(current),
            }
        }
        Err(Error::InvalidObject(format!(
            "replace depth too high for object {id}"
        )))
    }

    /// List valid direct object replacements in original-ID order.
    ///
    /// Like Git, replace refs with non-object-ID suffixes are ignored.
    ///
    /// # Errors
    /// Returns an error for malformed references or storage failures.
    pub fn replacements(&self) -> Result<Vec<Replacement>> {
        let map = self.load_replacements(false)?;
        Ok(map
            .into_iter()
            .map(|(original, replacement)| Replacement {
                original,
                replacement,
            })
            .collect())
    }

    /// Create or update a replace ref after validating both raw objects.
    ///
    /// Without `force`, the object kinds must match and an existing replace ref
    /// cannot be overwritten.
    ///
    /// # Errors
    /// Returns an error for missing objects, kind mismatch, an existing ref,
    /// a stale concurrent update, malformed storage, or filesystem failure.
    pub fn create_replacement(
        &self,
        original: ObjectId,
        replacement: ObjectId,
        force: bool,
        max_object_size: usize,
    ) -> Result<Replacement> {
        let original_kind = self.read_object_raw(original, max_object_size)?.kind();
        let replacement_kind = self.read_object_raw(replacement, max_object_size)?.kind();
        if !force && original_kind != replacement_kind {
            return Err(Error::InvalidObject(format!(
                "replacement kind {replacement_kind:?} does not match original kind {original_kind:?}"
            )));
        }
        let name = replace_name(original)?;
        let previous = match self.read_reference(name.as_str()) {
            Ok(reference) => match reference.target() {
                ReferenceTarget::Direct(id) if force => PreviousValue::MustExist(*id),
                ReferenceTarget::Direct(_) | ReferenceTarget::Symbolic(_) => {
                    return Err(Error::AlreadyExists(name.as_str().into()));
                }
            },
            Err(Error::NotFound(_)) => PreviousValue::MustNotExist,
            Err(error) => return Err(error),
        };
        self.update_reference(&name, replacement, previous)?;
        self.invalidate_replacements()?;
        Ok(Replacement {
            original,
            replacement,
        })
    }

    /// Delete an object replacement with a compare-and-swap precondition.
    ///
    /// # Errors
    /// Returns an error when the ref is missing, symbolic, concurrently changed,
    /// malformed, or cannot be removed from storage.
    pub fn delete_replacement(&self, original: ObjectId) -> Result<Replacement> {
        let name = replace_name(original)?;
        let reference = self.read_reference(name.as_str())?;
        let ReferenceTarget::Direct(replacement) = reference.target() else {
            return Err(Error::InvalidReference(format!("{name} is symbolic")));
        };
        self.delete_reference(&name, *replacement)?;
        self.invalidate_replacements()?;
        Ok(Replacement {
            original,
            replacement: *replacement,
        })
    }

    /// Drop the lazy replace cache so changes made through another storage
    /// client become visible to subsequent object reads.
    ///
    /// # Errors
    /// Returns an error if another thread poisoned the cache lock.
    pub fn invalidate_replacements(&self) -> Result<()> {
        *self
            .replacements
            .write()
            .map_err(|_| Error::InvalidRepository("replace cache lock poisoned".into()))? = None;
        Ok(())
    }

    fn prepare_replacements(&self) -> Result<()> {
        if self
            .replacements
            .read()
            .map_err(|_| Error::InvalidRepository("replace cache lock poisoned".into()))?
            .is_some()
        {
            return Ok(());
        }
        let loaded = self.load_replacements(true)?;
        let mut cache = self
            .replacements
            .write()
            .map_err(|_| Error::InvalidRepository("replace cache lock poisoned".into()))?;
        if cache.is_none() {
            *cache = Some(loaded);
        }
        Ok(())
    }

    fn load_replacements(&self, honor_config: bool) -> Result<BTreeMap<ObjectId, ObjectId>> {
        if honor_config {
            let config = self.read_config()?;
            if config.get("core.useReplaceRefs")?.is_some()
                && !config.get_bool("core.useReplaceRefs")?
            {
                return Ok(BTreeMap::new());
            }
        }
        let mut output = BTreeMap::new();
        for reference in self.references()? {
            let Some(suffix) = reference.name().strip_prefix(REPLACE_PREFIX) else {
                continue;
            };
            let Ok(original) = ObjectId::from_str(suffix) else {
                continue;
            };
            let ReferenceTarget::Direct(replacement) = reference.target() else {
                continue;
            };
            if output.insert(original, *replacement).is_some() {
                return Err(Error::InvalidReference(format!(
                    "duplicate replace ref for {original}"
                )));
            }
        }
        Ok(output)
    }
}

fn replace_name(original: ObjectId) -> Result<ReferenceName> {
    ReferenceName::new(format!("{REPLACE_PREFIX}{original}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InitOptions, MemoryFileSystem, ObjectKind};

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }

    #[test]
    fn replacement_is_transparent_raw_is_not_and_delete_restores_original() {
        let repository = repository();
        let original = repository.write_object(ObjectKind::Blob, b"old").unwrap();
        let replacement = repository.write_object(ObjectKind::Blob, b"new").unwrap();
        repository
            .create_replacement(original, replacement, false, 1024)
            .unwrap();
        assert_eq!(
            repository.read_object(original, 1024).unwrap().data(),
            b"new"
        );
        assert_eq!(
            repository.read_object_raw(original, 1024).unwrap().data(),
            b"old"
        );
        assert_eq!(repository.replacements().unwrap().len(), 1);
        repository.delete_replacement(original).unwrap();
        assert_eq!(
            repository.read_object(original, 1024).unwrap().data(),
            b"old"
        );
    }

    #[test]
    fn follows_chains_bounds_cycles_and_honors_config() {
        let repository = repository();
        let ids: Vec<_> = (0..7)
            .map(|index| repository.write_object(ObjectKind::Blob, &[index]).unwrap())
            .collect();
        for pair in ids.windows(2).take(5) {
            repository
                .create_replacement(pair[0], pair[1], false, 1024)
                .unwrap();
        }
        assert!(matches!(
            repository.resolve_replacement(ids[0]),
            Err(Error::InvalidObject(_))
        ));
        let mut config = repository.read_config().unwrap();
        config.set("core.useReplaceRefs", b"false").unwrap();
        repository.write_config(&config).unwrap();
        repository.invalidate_replacements().unwrap();
        assert_eq!(repository.resolve_replacement(ids[0]).unwrap(), ids[0]);
    }

    #[test]
    fn requires_force_for_kind_change_and_existing_ref() {
        let repository = repository();
        let blob = repository.write_object(ObjectKind::Blob, b"blob").unwrap();
        let tree = repository.write_object(ObjectKind::Tree, b"").unwrap();
        assert!(
            repository
                .create_replacement(blob, tree, false, 1024)
                .is_err()
        );
        repository
            .create_replacement(blob, tree, true, 1024)
            .unwrap();
        assert!(
            repository
                .create_replacement(blob, tree, false, 1024)
                .is_err()
        );
    }
}
