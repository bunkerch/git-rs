//! Create validated commit objects without updating a reference.

use std::collections::BTreeSet;

use crate::{
    CommitBuilder, Error, ExtraHeader, ObjectId, ObjectKind, Repository, Result, Signature,
};

/// One message source in command-line order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitTreeMessagePart {
    /// A `-m` paragraph: completed with LF and separated by one blank line.
    Paragraph(Vec<u8>),
    /// Exact `-F`/standard-input bytes, separated from prior content by one LF.
    FileContents(Vec<u8>),
}

/// Signing backend for commit-tree object creation.
pub trait CommitSigner {
    /// Return a detached textual signature for the exact unsigned commit body.
    ///
    /// # Errors
    /// Returns a backend-specific signing error.
    fn sign(&self, unsigned_commit: &[u8]) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTreeOptions {
    /// Commit encoding. UTF-8 aliases omit the encoding header and repair
    /// invalid bytes as Latin-1; other values preserve message bytes.
    pub encoding: Option<Vec<u8>>,
    pub max_parents: usize,
    pub max_message_size: usize,
    pub max_object_size: usize,
}

impl Default for CommitTreeOptions {
    fn default() -> Self {
        Self {
            encoding: None,
            max_parents: 1_000_000,
            max_message_size: 1024 * 1024 * 1024,
            max_object_size: 1024 * 1024 * 1024,
        }
    }
}

impl Repository {
    /// Create a commit object from an existing tree and zero or more parents.
    ///
    /// This plumbing operation does not update `HEAD` or any reference.
    /// Duplicate parents are ignored after their first occurrence.
    ///
    /// # Errors
    /// Returns an error for wrong or missing object types, exceeded limits,
    /// NUL in the message, an invalid encoding name, or storage failure.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_tree(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message_parts: &[CommitTreeMessagePart],
        author: &Signature,
        committer: &Signature,
        options: &CommitTreeOptions,
    ) -> Result<ObjectId> {
        self.commit_tree_inner(
            tree,
            parents,
            message_parts,
            author,
            committer,
            options,
            None,
        )
    }

    /// Create a signed commit object through a caller-provided signing backend.
    ///
    /// The signer receives the complete unsigned commit body and its returned
    /// text is encoded as Git's multiline `gpgsig` header.
    ///
    /// # Errors
    /// Returns the errors documented by [`Self::commit_tree`], signer errors,
    /// or an error for an empty, NUL-containing signature.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_tree_signed(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message_parts: &[CommitTreeMessagePart],
        author: &Signature,
        committer: &Signature,
        options: &CommitTreeOptions,
        signer: &dyn CommitSigner,
    ) -> Result<ObjectId> {
        self.commit_tree_inner(
            tree,
            parents,
            message_parts,
            author,
            committer,
            options,
            Some(signer),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_tree_inner(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message_parts: &[CommitTreeMessagePart],
        author: &Signature,
        committer: &Signature,
        options: &CommitTreeOptions,
        signer: Option<&dyn CommitSigner>,
    ) -> Result<ObjectId> {
        require_kind(
            self,
            tree,
            ObjectKind::Tree,
            options.max_object_size,
            "tree",
        )?;
        let mut unique = Vec::with_capacity(parents.len());
        let mut seen = BTreeSet::new();
        for parent in parents.iter().copied() {
            if seen.insert(parent) {
                if unique.len() == options.max_parents {
                    return commit_error("commit-tree parent count exceeds limit");
                }
                require_kind(
                    self,
                    parent,
                    ObjectKind::Commit,
                    options.max_object_size,
                    "parent",
                )?;
                unique.push(parent);
            }
        }
        let mut message = assemble_message(message_parts, options.max_message_size)?;
        let encoding = options.encoding.as_deref();
        if encoding.is_none_or(is_utf8_encoding) {
            message = repair_utf8_as_latin1(&message, options.max_message_size)?;
        }
        let mut builder = CommitBuilder::new(tree, author.clone(), committer.clone());
        for parent in unique {
            builder = builder.parent(parent);
        }
        if let Some(encoding) = encoding.filter(|value| !is_utf8_encoding(value)) {
            if encoding.is_empty()
                || encoding
                    .iter()
                    .any(|byte| matches!(*byte, b'\n' | b'\r' | 0))
            {
                return commit_error("invalid commit encoding name");
            }
            builder = builder.extra_header(ExtraHeader::new(b"encoding".to_vec(), encoding)?);
        }
        if let Some(signer) = signer {
            let unsigned = builder.clone().message(message.clone()).build().encode();
            let signature = signature_header_value(&signer.sign(&unsigned)?)?;
            builder = builder.extra_header(ExtraHeader::new(b"gpgsig".to_vec(), signature)?);
        }
        self.write_commit(&builder.message(message).build())
    }
}

fn signature_header_value(signature: &[u8]) -> Result<Vec<u8>> {
    if signature.is_empty() || signature.contains(&0) {
        return commit_error("empty or NUL-containing commit signature");
    }
    let signature = signature.strip_suffix(b"\n").unwrap_or(signature);
    let mut value = Vec::with_capacity(signature.len() + 16);
    for (index, line) in signature.split(|byte| *byte == b'\n').enumerate() {
        if index != 0 {
            value.extend_from_slice(b"\n ");
        }
        value.extend_from_slice(line);
    }
    Ok(value)
}

fn require_kind(
    repository: &Repository,
    id: ObjectId,
    expected: ObjectKind,
    max_size: usize,
    label: &str,
) -> Result<()> {
    let object = repository.read_object(id, max_size)?;
    if object.kind() != expected {
        return commit_error(format!("{label} {id} is not a {expected:?} object"));
    }
    Ok(())
}

fn assemble_message(parts: &[CommitTreeMessagePart], max_size: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for part in parts {
        if !output.is_empty() {
            checked_push(&mut output, b'\n', max_size)?;
        }
        match part {
            CommitTreeMessagePart::Paragraph(value) => {
                checked_extend(&mut output, value, max_size)?;
                if !output.ends_with(b"\n") {
                    checked_push(&mut output, b'\n', max_size)?;
                }
            }
            CommitTreeMessagePart::FileContents(value) => {
                checked_extend(&mut output, value, max_size)?;
            }
        }
    }
    if output.contains(&0) {
        return commit_error("NUL byte in commit message");
    }
    Ok(output)
}

fn checked_push(output: &mut Vec<u8>, byte: u8, max_size: usize) -> Result<()> {
    if output.len() == max_size {
        return commit_error("commit-tree message exceeds limit");
    }
    output.push(byte);
    Ok(())
}

fn checked_extend(output: &mut Vec<u8>, value: &[u8], max_size: usize) -> Result<()> {
    if value.len() > max_size.saturating_sub(output.len()) {
        return commit_error("commit-tree message exceeds limit");
    }
    output.extend_from_slice(value);
    Ok(())
}

fn is_utf8_encoding(value: &[u8]) -> bool {
    value.eq_ignore_ascii_case(b"utf-8") || value.eq_ignore_ascii_case(b"utf8")
}

fn repair_utf8_as_latin1(input: &[u8], max_size: usize) -> Result<Vec<u8>> {
    let mut remaining = input;
    let mut output = Vec::with_capacity(input.len());
    loop {
        match std::str::from_utf8(remaining) {
            Ok(_) => {
                checked_extend(&mut output, remaining, max_size)?;
                return Ok(output);
            }
            Err(error) => {
                let valid = error.valid_up_to();
                checked_extend(&mut output, &remaining[..valid], max_size)?;
                let byte = remaining[valid];
                checked_extend(
                    &mut output,
                    &[0xc0 | (byte >> 6), 0x80 | (byte & 0x3f)],
                    max_size,
                )?;
                remaining = &remaining[valid + 1..];
            }
        }
    }
}

fn commit_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidCommit(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{CommitSigner, CommitTreeMessagePart, CommitTreeOptions};
    use crate::{
        CommitBuilder, InitOptions, MemoryFileSystem, ObjectKind, Repository, Signature, Tree,
    };

    #[test]
    fn creates_validated_commit_and_deduplicates_parents_in_order() {
        let repository = repository();
        let signature = signature();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let first = repository
            .write_commit(&CommitBuilder::new(tree, signature.clone(), signature.clone()).build())
            .unwrap();
        let second = repository
            .write_commit(
                &CommitBuilder::new(tree, signature.clone(), signature.clone())
                    .parent(first)
                    .build(),
            )
            .unwrap();
        let id = repository
            .commit_tree(
                tree,
                &[first, first, second],
                &[
                    CommitTreeMessagePart::Paragraph(b"subject".to_vec()),
                    CommitTreeMessagePart::FileContents(b"body\n".to_vec()),
                ],
                &signature,
                &signature,
                &CommitTreeOptions::default(),
            )
            .unwrap();
        let commit = repository.read_commit(id, 4096).unwrap();
        assert_eq!(commit.parents(), &[first, second]);
        assert_eq!(commit.message(), b"subject\n\nbody\n");
    }

    #[test]
    fn validates_types_limits_nul_and_non_utf8_encoding_policy() {
        let repository = repository();
        let signature = signature();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let blob = repository.write_object(ObjectKind::Blob, b"blob").unwrap();
        assert!(
            repository
                .commit_tree(
                    blob,
                    &[],
                    &[],
                    &signature,
                    &signature,
                    &CommitTreeOptions::default()
                )
                .is_err()
        );
        assert!(
            repository
                .commit_tree(
                    tree,
                    &[],
                    &[CommitTreeMessagePart::FileContents(
                        b"bad\0message".to_vec()
                    )],
                    &signature,
                    &signature,
                    &CommitTreeOptions::default()
                )
                .is_err()
        );
        let repaired = repository
            .commit_tree(
                tree,
                &[],
                &[CommitTreeMessagePart::FileContents(vec![b'm', 0xe9])],
                &signature,
                &signature,
                &CommitTreeOptions::default(),
            )
            .unwrap();
        assert_eq!(
            repository.read_commit(repaired, 4096).unwrap().message(),
            "mé".as_bytes()
        );
        let latin1 = repository
            .commit_tree(
                tree,
                &[],
                &[CommitTreeMessagePart::FileContents(vec![b'm', 0xe9])],
                &signature,
                &signature,
                &CommitTreeOptions {
                    encoding: Some(b"ISO-8859-1".to_vec()),
                    ..CommitTreeOptions::default()
                },
            )
            .unwrap();
        let commit = repository.read_commit(latin1, 4096).unwrap();
        assert_eq!(commit.message(), &[b'm', 0xe9]);
        assert_eq!(commit.extra_headers()[0].name(), b"encoding");
    }

    #[test]
    fn signs_the_exact_unsigned_body_and_formats_multiline_header() {
        struct Signer;
        impl CommitSigner for Signer {
            fn sign(&self, unsigned_commit: &[u8]) -> crate::Result<Vec<u8>> {
                assert!(unsigned_commit.starts_with(b"tree "));
                assert!(unsigned_commit.ends_with(b"\n\nmessage\n"));
                Ok(b"-----BEGIN SIGNATURE-----\nbody\n-----END SIGNATURE-----\n".to_vec())
            }
        }

        let repository = repository();
        let signature = signature();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let id = repository
            .commit_tree_signed(
                tree,
                &[],
                &[CommitTreeMessagePart::Paragraph(b"message".to_vec())],
                &signature,
                &signature,
                &CommitTreeOptions::default(),
                &Signer,
            )
            .unwrap();
        assert_eq!(id.to_string(), "8d334ab05b2419e48e6dfb79fdedc034837e380d");
        let commit = repository.read_commit(id, 4096).unwrap();
        let header = &commit.extra_headers()[0];
        assert_eq!(header.name(), b"gpgsig");
        assert_eq!(
            header.value(),
            b"-----BEGIN SIGNATURE-----\n body\n -----END SIGNATURE-----"
        );
    }

    fn repository() -> Repository {
        Repository::init(
            MemoryFileSystem::new(),
            ".",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap()
    }

    fn signature() -> Signature {
        Signature::new("Author", "author@example.com", 1_700_000_000, 90).unwrap()
    }
}
