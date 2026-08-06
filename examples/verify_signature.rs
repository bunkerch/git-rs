use std::env;
use std::str::FromStr;

use git_rs::{
    HostFileSystem, ObjectId, Repository, Result, SignatureFormat, SignatureVerification,
    SignatureVerifier, VerifySignatureOptions,
};

struct InspectionVerifier;

impl SignatureVerifier for InspectionVerifier {
    fn verify(
        &self,
        _format: SignatureFormat,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<SignatureVerification> {
        Ok(SignatureVerification {
            // This example only demonstrates extraction. A real backend must
            // set validity from its cryptographic verification result.
            valid: false,
            signer: None,
            key_id: None,
            status: format!("payload={} signature={}", payload.len(), signature.len()).into_bytes(),
        })
    }
}

fn main() -> Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let kind = env::args()
        .nth(2)
        .expect("usage: verify_signature <repository> <commit|tag> <object-id>");
    let id = ObjectId::from_str(
        &env::args()
            .nth(3)
            .expect("usage: verify_signature <repository> <commit|tag> <object-id>"),
    )?;
    let repository = Repository::open(HostFileSystem::new(&repository_path)?, ".")?;
    let result = match kind.as_str() {
        "commit" => repository.verify_commit_signature(
            id,
            &InspectionVerifier,
            &VerifySignatureOptions::default(),
        )?,
        "tag" => repository.verify_tag_signature(
            id,
            &InspectionVerifier,
            &VerifySignatureOptions::default(),
        )?,
        _ => {
            return Err(git_rs::Error::InvalidRepository(
                "kind must be `commit` or `tag`".into(),
            ));
        }
    };
    println!(
        "format={:?} payload={} signature={} valid={}",
        result.format,
        result.payload.len(),
        result.signature.len(),
        result.verification.valid
    );
    Ok(())
}
