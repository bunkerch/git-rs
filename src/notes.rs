//! Git-compatible notes stored below `refs/notes/`.

use std::collections::BTreeMap;
use std::str::FromStr;

use crate::{
    CommitBuilder, EntryMode, Error, ObjectId, ObjectKind, PreviousValue, ReferenceName,
    Repository, Result, Signature, Tree, TreeEntry,
};

pub const DEFAULT_NOTES_REF: &str = "refs/notes/commits";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotesOptions {
    pub reference: String,
    pub max_object_size: usize,
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Default for NotesOptions {
    fn default() -> Self {
        Self {
            reference: DEFAULT_NOTES_REF.into(),
            max_object_size: 64 * 1024 * 1024,
            max_entries: 1_000_000,
            max_depth: 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Note {
    target: ObjectId,
    blob: ObjectId,
    message: Vec<u8>,
}

impl Note {
    #[must_use]
    pub const fn target(&self) -> ObjectId {
        self.target
    }
    #[must_use]
    pub const fn blob(&self) -> ObjectId {
        self.blob
    }
    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }
}

#[derive(Clone)]
struct StoredEntry {
    path: Vec<Vec<u8>>,
    mode: EntryMode,
    id: ObjectId,
}

struct NoteCommit<'a> {
    author: &'a Signature,
    committer: &'a Signature,
    options: &'a NotesOptions,
}

impl Repository {
    /// List notes in object-ID order. Both flat and fanout trees are accepted.
    ///
    /// # Errors
    /// Returns an error for an invalid ref, corrupt notes commit/tree, duplicate
    /// target, non-blob note, exceeded resource bound, or storage failure.
    pub fn list_notes(&self, options: &NotesOptions) -> Result<Vec<Note>> {
        let (_, entries) = self.load_notes(options)?;
        let mut notes = Vec::new();
        for entry in entries {
            if let Some(target) = note_target(&entry.path) {
                if entry.mode != EntryMode::Blob {
                    return Err(Error::InvalidTree(format!("note {target} is not a blob")));
                }
                let object = self.read_object(entry.id, options.max_object_size)?;
                if object.kind() != ObjectKind::Blob {
                    return Err(Error::InvalidObject(format!(
                        "note {target} does not name a blob"
                    )));
                }
                notes.push(Note {
                    target,
                    blob: entry.id,
                    message: object.data().to_vec(),
                });
            }
        }
        notes.sort_unstable_by_key(Note::target);
        for pair in notes.windows(2) {
            if pair[0].target == pair[1].target {
                return Err(Error::InvalidTree(format!(
                    "duplicate note for {}",
                    pair[0].target
                )));
            }
        }
        Ok(notes)
    }

    /// Read one note, returning `None` when the notes ref or entry is absent.
    ///
    /// # Errors
    /// Returns the same errors as [`Repository::list_notes`].
    pub fn note(&self, target: ObjectId, options: &NotesOptions) -> Result<Option<Note>> {
        Ok(self
            .list_notes(options)?
            .into_iter()
            .find(|note| note.target == target))
    }

    /// Add or replace a note and atomically advance its notes ref.
    ///
    /// # Errors
    /// Returns an error when the target is missing, an existing note may not be
    /// replaced, stored notes are invalid, the ref races, or storage fails.
    pub fn add_note(
        &self,
        target: ObjectId,
        message: &[u8],
        force: bool,
        author: &Signature,
        committer: &Signature,
        options: &NotesOptions,
    ) -> Result<Note> {
        self.read_object(target, options.max_object_size)?;
        let blob = self.write_object(ObjectKind::Blob, message)?;
        let commit = NoteCommit {
            author,
            committer,
            options,
        };
        self.change_note(
            target,
            Some(blob),
            force,
            b"Notes added by git-rs\n",
            &commit,
        )?;
        Ok(Note {
            target,
            blob,
            message: message.to_vec(),
        })
    }

    /// Copy a note between annotated objects.
    ///
    /// # Errors
    /// Returns an error when either required object/note is missing, replacement
    /// is disallowed, stored notes are invalid, the ref races, or storage fails.
    pub fn copy_note(
        &self,
        source: ObjectId,
        target: ObjectId,
        force: bool,
        author: &Signature,
        committer: &Signature,
        options: &NotesOptions,
    ) -> Result<Note> {
        self.read_object(target, options.max_object_size)?;
        let note = self
            .note(source, options)?
            .ok_or_else(|| Error::InvalidObject(format!("no note found for {source}")))?;
        let commit = NoteCommit {
            author,
            committer,
            options,
        };
        self.change_note(
            target,
            Some(note.blob),
            force,
            b"Notes copied by git-rs\n",
            &commit,
        )?;
        Ok(Note {
            target,
            blob: note.blob,
            message: note.message,
        })
    }

    /// Remove a note and atomically advance its notes ref.
    ///
    /// # Errors
    /// Returns an error when the note is absent, stored notes are invalid, the
    /// ref races, or storage fails.
    pub fn remove_note(
        &self,
        target: ObjectId,
        author: &Signature,
        committer: &Signature,
        options: &NotesOptions,
    ) -> Result<()> {
        let commit = NoteCommit {
            author,
            committer,
            options,
        };
        self.change_note(target, None, false, b"Notes removed by git-rs\n", &commit)
    }

    fn change_note(
        &self,
        target: ObjectId,
        replacement: Option<ObjectId>,
        force: bool,
        message: &[u8],
        commit_options: &NoteCommit<'_>,
    ) -> Result<()> {
        let NoteCommit {
            author,
            committer,
            options,
        } = *commit_options;
        let name = notes_ref(options)?;
        let (old, mut entries) = self.load_notes(options)?;
        let matching: Vec<_> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (note_target(&entry.path) == Some(target)).then_some(index)
            })
            .collect();
        if matching.len() > 1 {
            return Err(Error::InvalidTree(format!("duplicate note for {target}")));
        }
        if replacement.is_some() && !matching.is_empty() && !force {
            return Err(Error::AlreadyExists(target.to_string().into()));
        }
        if replacement.is_none() && matching.is_empty() {
            return Err(Error::NotFound(target.to_string().into()));
        }
        let old_path = matching.first().map(|index| entries[*index].path.clone());
        entries.retain(|entry| note_target(&entry.path) != Some(target));
        if let Some(id) = replacement {
            let path = old_path.unwrap_or_else(|| fanout_path(target, &entries));
            entries.push(StoredEntry {
                path,
                mode: EntryMode::Blob,
                id,
            });
        }
        let tree = self.write_stored_tree(&entries)?;
        let mut builder =
            CommitBuilder::new(tree, author.clone(), committer.clone()).message(message);
        if let Some(parent) = old {
            builder = builder.parent(parent);
        }
        let commit = self.write_commit(&builder.build())?;
        let previous = old.map_or(PreviousValue::MustNotExist, PreviousValue::MustExist);
        let reflog_message = message.strip_suffix(b"\n").unwrap_or(message);
        self.update_reference_with_reflog(&name, commit, previous, committer, reflog_message)
    }

    fn load_notes(&self, options: &NotesOptions) -> Result<(Option<ObjectId>, Vec<StoredEntry>)> {
        let name = notes_ref(options)?;
        let commit_id = match self.resolve_reference(name.as_str()) {
            Ok(id) => id,
            Err(Error::NotFound(_)) => return Ok((None, Vec::new())),
            Err(error) => return Err(error),
        };
        let commit = self.read_commit(commit_id, options.max_object_size)?;
        let mut entries = Vec::new();
        self.collect_stored_entries(commit.tree(), &mut Vec::new(), &mut entries, options, 0)?;
        Ok((Some(commit_id), entries))
    }

    fn collect_stored_entries(
        &self,
        tree_id: ObjectId,
        prefix: &mut Vec<Vec<u8>>,
        output: &mut Vec<StoredEntry>,
        options: &NotesOptions,
        depth: usize,
    ) -> Result<()> {
        if depth > options.max_depth {
            return Err(Error::InvalidTree("notes tree exceeds depth limit".into()));
        }
        let tree = self.read_tree(tree_id, options.max_object_size)?;
        for entry in tree.entries() {
            if output.len() >= options.max_entries {
                return Err(Error::InvalidTree("notes tree exceeds entry limit".into()));
            }
            prefix.push(entry.name().to_vec());
            if entry.mode() == EntryMode::Tree {
                self.collect_stored_entries(entry.id(), prefix, output, options, depth + 1)?;
            } else {
                output.push(StoredEntry {
                    path: prefix.clone(),
                    mode: entry.mode(),
                    id: entry.id(),
                });
            }
            prefix.pop();
        }
        Ok(())
    }

    fn write_stored_tree(&self, entries: &[StoredEntry]) -> Result<ObjectId> {
        let mut root = Node::default();
        for entry in entries {
            root.insert(&entry.path, entry.mode, entry.id)?;
        }
        root.write(self)
    }
}

fn notes_ref(options: &NotesOptions) -> Result<ReferenceName> {
    let name = ReferenceName::new(options.reference.clone())?;
    if !name.as_str().starts_with("refs/notes/") {
        return Err(Error::InvalidReferenceName(options.reference.clone()));
    }
    Ok(name)
}

fn note_target(path: &[Vec<u8>]) -> Option<ObjectId> {
    let bytes: Vec<u8> = path.iter().flatten().copied().collect();
    if bytes.len() != ObjectId::HEX_LENGTH || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    ObjectId::from_str(std::str::from_utf8(&bytes).ok()?).ok()
}

fn fanout_path(target: ObjectId, entries: &[StoredEntry]) -> Vec<Vec<u8>> {
    let hex = target.to_string().into_bytes();
    let Some(model) = entries
        .iter()
        .find(|entry| note_target(&entry.path).is_some())
    else {
        return vec![hex];
    };
    let mut start = 0;
    model
        .path
        .iter()
        .map(|part| {
            let end = start + part.len();
            let component = hex[start..end].to_vec();
            start = end;
            component
        })
        .collect()
}

#[derive(Default)]
struct Node {
    leaves: BTreeMap<Vec<u8>, (EntryMode, ObjectId)>,
    directories: BTreeMap<Vec<u8>, Node>,
}

impl Node {
    fn insert(&mut self, path: &[Vec<u8>], mode: EntryMode, id: ObjectId) -> Result<()> {
        let (name, rest) = path
            .split_first()
            .ok_or_else(|| Error::InvalidTree("empty notes path".into()))?;
        if rest.is_empty() {
            if self.directories.contains_key(name)
                || self.leaves.insert(name.clone(), (mode, id)).is_some()
            {
                return Err(Error::InvalidTree("notes tree path collision".into()));
            }
        } else {
            if self.leaves.contains_key(name) {
                return Err(Error::InvalidTree("notes tree path collision".into()));
            }
            self.directories
                .entry(name.clone())
                .or_default()
                .insert(rest, mode, id)?;
        }
        Ok(())
    }

    fn write(&self, repository: &Repository) -> Result<ObjectId> {
        let mut entries = Vec::with_capacity(self.leaves.len() + self.directories.len());
        for (name, (mode, id)) in &self.leaves {
            entries.push(TreeEntry::new(*mode, name.clone(), *id)?);
        }
        for (name, node) in &self.directories {
            entries.push(TreeEntry::new(
                EntryMode::Tree,
                name.clone(),
                node.write(repository)?,
            )?);
        }
        repository.write_tree(&Tree::new(entries)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InitOptions, MemoryFileSystem};

    fn fixture() -> (Repository, Signature, ObjectId, ObjectId) {
        let repository =
            Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap();
        let signature = Signature::new("A U Thor", "author@example.com", 1, 0).unwrap();
        let first = repository.write_object(ObjectKind::Blob, b"first").unwrap();
        let second = repository
            .write_object(ObjectKind::Blob, b"second")
            .unwrap();
        (repository, signature, first, second)
    }

    #[test]
    fn adds_replaces_copies_and_removes_notes() {
        let (repository, signature, first, second) = fixture();
        let options = NotesOptions::default();
        repository
            .add_note(first, b"hello\n", false, &signature, &signature, &options)
            .unwrap();
        assert_eq!(
            repository.note(first, &options).unwrap().unwrap().message(),
            b"hello\n"
        );
        assert!(
            repository
                .add_note(first, b"no", false, &signature, &signature, &options)
                .is_err()
        );
        repository
            .add_note(first, b"replaced", true, &signature, &signature, &options)
            .unwrap();
        repository
            .copy_note(first, second, false, &signature, &signature, &options)
            .unwrap();
        assert_eq!(repository.list_notes(&options).unwrap().len(), 2);
        repository
            .remove_note(first, &signature, &signature, &options)
            .unwrap();
        assert!(repository.note(first, &options).unwrap().is_none());
        assert_eq!(
            repository
                .note(second, &options)
                .unwrap()
                .unwrap()
                .message(),
            b"replaced"
        );
    }

    #[test]
    fn reads_fanout_and_preserves_unrelated_entries() {
        let (repository, signature, first, second) = fixture();
        let note_blob = repository
            .write_object(ObjectKind::Blob, b"fanout")
            .unwrap();
        let hex = first.to_string();
        let subtree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, hex.as_bytes()[2..].to_vec(), note_blob)
                        .unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let unrelated = repository.write_object(ObjectKind::Blob, b"keep").unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Tree, hex.as_bytes()[..2].to_vec(), subtree).unwrap(),
                    TreeEntry::new(EntryMode::Blob, b"README".to_vec(), unrelated).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let commit = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .message(b"seed\n")
                    .build(),
            )
            .unwrap();
        let name = ReferenceName::new(DEFAULT_NOTES_REF).unwrap();
        repository
            .update_reference(&name, commit, PreviousValue::MustNotExist)
            .unwrap();
        let options = NotesOptions::default();
        assert_eq!(
            repository.note(first, &options).unwrap().unwrap().message(),
            b"fanout"
        );
        repository
            .add_note(second, b"new", false, &signature, &signature, &options)
            .unwrap();
        let head = repository.resolve_reference(DEFAULT_NOTES_REF).unwrap();
        let new_tree = repository
            .read_tree(
                repository
                    .read_commit(head, options.max_object_size)
                    .unwrap()
                    .tree(),
                options.max_object_size,
            )
            .unwrap();
        assert!(
            new_tree
                .entries()
                .iter()
                .any(|entry| entry.name() == b"README")
        );
        assert_eq!(repository.list_notes(&options).unwrap().len(), 2);
    }
}
