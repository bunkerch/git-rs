//! Exact signed-object payload extraction with pluggable verification.

use crate::{AnnotatedTag, Commit, Error, ObjectId, ObjectKind, Repository, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureFormat {
    OpenPgp,
    X509,
    Ssh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignedObjectKind {
    Commit,
    Tag,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureVerification {
    pub valid: bool,
    pub signer: Option<Vec<u8>>,
    pub key_id: Option<Vec<u8>>,
    pub status: Vec<u8>,
}

pub trait SignatureVerifier {
    /// Verify a detached signature over the exact canonical payload.
    ///
    /// # Errors
    /// Returns a backend or policy error. Cryptographic invalidity should
    /// normally be represented by `valid: false` in the returned result.
    fn verify(
        &self,
        format: SignatureFormat,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<SignatureVerification>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifySignatureOptions {
    pub max_object_size: usize,
    pub max_payload_size: usize,
    pub max_signature_size: usize,
}

impl Default for VerifySignatureOptions {
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024 * 1024,
            max_payload_size: 1024 * 1024 * 1024,
            max_signature_size: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSignature {
    pub object: ObjectId,
    pub kind: SignedObjectKind,
    pub format: SignatureFormat,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
    pub verification: SignatureVerification,
}

impl Repository {
    /// Extract and verify a signed commit's canonical unsigned body.
    ///
    /// # Errors
    /// Returns an error for missing/non-commit/malformed/unsigned objects,
    /// unknown armor, limits, storage failures, or verifier backend errors.
    pub fn verify_commit_signature(
        &self,
        id: ObjectId,
        verifier: &dyn SignatureVerifier,
        options: &VerifySignatureOptions,
    ) -> Result<VerifiedSignature> {
        self.verify_object_signature(id, SignedObjectKind::Commit, verifier, options)
    }

    /// Extract and verify an annotated tag's embedded armored signature.
    ///
    /// # Errors
    /// Returns the same classes of errors as [`Self::verify_commit_signature`].
    pub fn verify_tag_signature(
        &self,
        id: ObjectId,
        verifier: &dyn SignatureVerifier,
        options: &VerifySignatureOptions,
    ) -> Result<VerifiedSignature> {
        self.verify_object_signature(id, SignedObjectKind::Tag, verifier, options)
    }

    fn verify_object_signature(
        &self,
        id: ObjectId,
        kind: SignedObjectKind,
        verifier: &dyn SignatureVerifier,
        options: &VerifySignatureOptions,
    ) -> Result<VerifiedSignature> {
        let object = self.read_object(id, options.max_object_size)?;
        let expected = match kind {
            SignedObjectKind::Commit => ObjectKind::Commit,
            SignedObjectKind::Tag => ObjectKind::Tag,
        };
        if object.kind() != expected {
            return Err(Error::InvalidObject(format!(
                "object {id} is not a signed {expected:?}"
            )));
        }
        let (payload, signature) = match kind {
            SignedObjectKind::Commit => extract_commit_signature(object.data())?,
            SignedObjectKind::Tag => extract_tag_signature(object.data())?,
        };
        enforce_limits(&payload, &signature, options)?;
        let format = signature_format(&signature)?;
        let verification = verifier.verify(format, &payload, &signature)?;
        Ok(VerifiedSignature {
            object: id,
            kind,
            format,
            payload,
            signature,
            verification,
        })
    }
}

fn extract_commit_signature(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    Commit::parse(data)?;
    let mut payload = Vec::with_capacity(data.len());
    let mut signature = Vec::new();
    let mut cursor = 0;
    let mut selected = false;
    let mut signature_header = false;
    while cursor < data.len() {
        let end = data[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(data.len(), |offset| cursor + offset + 1);
        let line = &data[cursor..end];
        if line == b"\n" {
            payload.extend_from_slice(&data[cursor..]);
            break;
        }
        if signature_header && line.starts_with(b" ") {
            signature.extend_from_slice(&line[1..]);
        } else if let Some(value) = line.strip_prefix(b"gpgsig ") {
            selected = true;
            signature_header = true;
            signature.extend_from_slice(value);
        } else if line.starts_with(b"gpgsig") {
            signature_header = true;
        } else {
            signature_header = false;
            payload.extend_from_slice(line);
        }
        cursor = end;
    }
    if !selected || signature.is_empty() {
        return Err(Error::InvalidObject(
            "commit contains no SHA-1 gpgsig header".into(),
        ));
    }
    Ok((payload, signature))
}

fn extract_tag_signature(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    AnnotatedTag::parse(data)?;
    let mut match_offset = None;
    let mut cursor = 0;
    while cursor < data.len() {
        if signature_format(&data[cursor..]).is_ok() {
            match_offset = Some(cursor);
        }
        cursor = data[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(data.len(), |offset| cursor + offset + 1);
    }
    let offset = match_offset
        .ok_or_else(|| Error::InvalidObject("tag contains no armored signature".into()))?;
    Ok((data[..offset].to_vec(), data[offset..].to_vec()))
}

fn signature_format(signature: &[u8]) -> Result<SignatureFormat> {
    if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----")
        || signature.starts_with(b"-----BEGIN PGP MESSAGE-----")
    {
        Ok(SignatureFormat::OpenPgp)
    } else if signature.starts_with(b"-----BEGIN SIGNED MESSAGE-----") {
        Ok(SignatureFormat::X509)
    } else if signature.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
        Ok(SignatureFormat::Ssh)
    } else {
        Err(Error::InvalidObject(
            "unknown or incompatible signature armor".into(),
        ))
    }
}

fn enforce_limits(
    payload: &[u8],
    signature: &[u8],
    options: &VerifySignatureOptions,
) -> Result<()> {
    for (value, limit, label) in [
        (payload, options.max_payload_size, "signed payload"),
        (signature, options.max_signature_size, "signature"),
    ] {
        if value.len() > limit {
            return Err(Error::ObjectTooLarge {
                declared: value.len() as u64,
                limit,
            });
        }
        if value.is_empty() {
            return Err(Error::InvalidObject(format!("empty {label}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitBuilder, ExtraHeader, InitOptions, MemoryFileSystem, Signature, TagBuilder, Tree,
    };

    struct ExpectedVerifier {
        payload: Vec<u8>,
        signature: Vec<u8>,
        valid: bool,
    }

    impl SignatureVerifier for ExpectedVerifier {
        fn verify(
            &self,
            _format: SignatureFormat,
            payload: &[u8],
            signature: &[u8],
        ) -> Result<SignatureVerification> {
            assert_eq!(payload, self.payload);
            assert_eq!(signature, self.signature);
            Ok(SignatureVerification {
                valid: self.valid,
                signer: Some(b"Test Signer".to_vec()),
                key_id: Some(b"01234567".to_vec()),
                status: b"backend status".to_vec(),
            })
        }
    }

    #[test]
    fn extracts_commit_header_signature_and_exact_unsigned_payload() {
        let repository = repository();
        let tree = repository.write_tree(&Tree::default()).unwrap();
        let identity = identity();
        let unsigned = CommitBuilder::new(tree, identity.clone(), identity.clone())
            .message(b"message\n".to_vec())
            .build();
        let signature = b"-----BEGIN PGP SIGNATURE-----\nbytes\n-----END PGP SIGNATURE-----\n";
        let header = b"-----BEGIN PGP SIGNATURE-----\n bytes\n -----END PGP SIGNATURE-----";
        let signed = CommitBuilder::new(tree, identity.clone(), identity)
            .extra_header(ExtraHeader::new(b"gpgsig".to_vec(), header.to_vec()).unwrap())
            .message(b"message\n".to_vec())
            .build();
        let id = repository.write_commit(&signed).unwrap();
        let result = repository
            .verify_commit_signature(
                id,
                &ExpectedVerifier {
                    payload: unsigned.encode(),
                    signature: signature.to_vec(),
                    valid: true,
                },
                &VerifySignatureOptions::default(),
            )
            .unwrap();
        assert_eq!(result.format, SignatureFormat::OpenPgp);
        assert!(result.verification.valid);
        assert_eq!(result.kind, SignedObjectKind::Commit);
    }

    #[test]
    fn extracts_last_armored_tag_signature_and_preserves_invalid_result() {
        let repository = repository();
        let target = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        let identity = identity();
        let prefix = b"release message\n";
        let signature = b"-----BEGIN SSH SIGNATURE-----\nbytes\n-----END SSH SIGNATURE-----\n";
        let tag = TagBuilder::new(target, ObjectKind::Blob, "v1", identity)
            .unwrap()
            .message([prefix.as_slice(), signature.as_slice()].concat())
            .build();
        let id = repository.write_tag(&tag, 4096).unwrap();
        let encoded = repository.read_object(id, 4096).unwrap().into_data();
        let offset = encoded
            .windows(signature.len())
            .position(|window| window == signature)
            .unwrap();
        let result = repository
            .verify_tag_signature(
                id,
                &ExpectedVerifier {
                    payload: encoded[..offset].to_vec(),
                    signature: signature.to_vec(),
                    valid: false,
                },
                &VerifySignatureOptions::default(),
            )
            .unwrap();
        assert_eq!(result.format, SignatureFormat::Ssh);
        assert!(!result.verification.valid);
    }

    #[test]
    fn rejects_unsigned_wrong_kind_unknown_armor_and_limits() {
        let repository = repository();
        let target = repository.write_object(ObjectKind::Blob, b"x").unwrap();
        assert!(
            repository
                .verify_commit_signature(
                    target,
                    &ExpectedVerifier {
                        payload: vec![],
                        signature: vec![],
                        valid: true
                    },
                    &VerifySignatureOptions::default(),
                )
                .is_err()
        );
        let tag = TagBuilder::new(target, ObjectKind::Blob, "v1", identity())
            .unwrap()
            .message(b"unsigned\n".to_vec())
            .build();
        let id = repository.write_tag(&tag, 4096).unwrap();
        let verifier = ExpectedVerifier {
            payload: vec![],
            signature: vec![],
            valid: true,
        };
        assert!(
            repository
                .verify_tag_signature(id, &verifier, &VerifySignatureOptions::default())
                .is_err()
        );
        let tag = TagBuilder::new(target, ObjectKind::Blob, "v2", identity())
            .unwrap()
            .message(b"-----BEGIN UNKNOWN SIGNATURE-----\nbytes\n".to_vec())
            .build();
        let unknown = repository.write_tag(&tag, 4096).unwrap();
        assert!(
            repository
                .verify_tag_signature(unknown, &verifier, &VerifySignatureOptions::default())
                .is_err()
        );
        let tag = TagBuilder::new(target, ObjectKind::Blob, "v3", identity())
            .unwrap()
            .message(b"-----BEGIN PGP SIGNATURE-----\nbytes\n".to_vec())
            .build();
        let limited = repository.write_tag(&tag, 4096).unwrap();
        assert!(
            repository
                .verify_tag_signature(
                    limited,
                    &verifier,
                    &VerifySignatureOptions {
                        max_signature_size: 1,
                        ..VerifySignatureOptions::default()
                    },
                )
                .is_err()
        );
    }

    fn repository() -> Repository {
        Repository::init(MemoryFileSystem::new(), "repo", &InitOptions::default()).unwrap()
    }

    fn identity() -> Signature {
        Signature::new("Signer", "signer@example.com", 1, 0).unwrap()
    }
}
