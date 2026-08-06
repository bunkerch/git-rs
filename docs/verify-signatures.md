# Verifying signed commits and tags

`Repository::verify_commit_signature` and `verify_tag_signature` separate Git
object parsing from cryptographic policy. The library extracts the exact signed
payload and detached signature, detects its format, then calls a user-provided
`SignatureVerifier`.

```rust
use git_rs::{Result, SignatureFormat, SignatureVerification, SignatureVerifier};

struct CorporateVerifier;

impl SignatureVerifier for CorporateVerifier {
    fn verify(
        &self,
        format: SignatureFormat,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<SignatureVerification> {
        // Pass these exact bytes to an in-process crypto/HSM implementation.
        let valid = my_policy_check(format, payload, signature);
        Ok(SignatureVerification {
            valid,
            signer: None,
            key_id: None,
            status: Vec::new(),
        })
    }
}
# fn my_policy_check(_: SignatureFormat, _: &[u8], _: &[u8]) -> bool { true }
```

Commit verification removes the SHA-1 `gpgsig` header and its space-prefixed
continuations while retaining every other header, newline, and message byte.
Alternate-hash signature headers are excluded from the SHA-1 signed payload as
Git requires. Tag verification locates the final recognized armored signature
in the tag message and signs everything before it.

Recognized formats are OpenPGP (`PGP SIGNATURE` and historical `PGP MESSAGE`),
X.509 (`SIGNED MESSAGE`), and SSH. Unknown armor is rejected before invoking
the verifier. The returned `VerifiedSignature` contains the object ID, kind,
format, exact payload/signature bytes, and backend result. `valid: false` is a
normal cryptographic outcome; backend failures use `Result::Err`.

Object inflation, extracted payloads, and signatures have independent bounds.
Objects can reside loose or in packs and are read through the configured
filesystem. The library links no crypto implementation and starts no process;
applications can use an in-process library, HSM, remote signing service, or
their own policy engine.

The extraction logic follows `commit.c:parse_buffer_signed_by_header`,
`gpg-interface.c:parse_signature`, `tag.c:gpg_verify_tag`, and the
`verify-commit` / `verify-tag` builtins. The example intentionally reports
`valid=false`: it demonstrates byte extraction, not fake cryptography.

```text
cargo run --example verify_signature -- repository commit <object-id>
```
