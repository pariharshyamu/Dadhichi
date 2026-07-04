//! A signed extension marketplace.
//!
//! Before a plugin is installed, its integrity and provenance are checked
//! cryptographically. A publisher signs `(id, version, wasm-hash)` with their
//! Ed25519 key; the host verifies the signature against a set of
//! [`TrustedPublishers`] and re-hashes the module bytes, so a tampered module or
//! an untrusted author is rejected before any code runs.

use crate::Manifest;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use thiserror::Error;

/// Errors from marketplace verification.
#[derive(Debug, Error)]
pub enum MarketError {
    /// The publisher's key is not in the trusted set.
    #[error("untrusted publisher")]
    UntrustedPublisher,
    /// The signature did not verify against the signed digest.
    #[error("invalid signature")]
    InvalidSignature,
    /// The module bytes do not match the signed hash.
    #[error("module hash mismatch (tampered package)")]
    HashMismatch,
    /// A key or signature was malformed.
    #[error("malformed key or signature")]
    Malformed,
}

/// A publisher-signed package: a manifest, the module's hash, and a signature
/// over both by the publisher's key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedPackage {
    /// The plugin manifest.
    pub manifest: Manifest,
    /// SHA-256 of the WASM module bytes.
    pub wasm_sha256: [u8; 32],
    /// The publisher's Ed25519 public key (32 bytes).
    pub publisher: Vec<u8>,
    /// The Ed25519 signature over the digest (64 bytes).
    pub signature: Vec<u8>,
}

/// The exact bytes that are signed: `id ‖ 0 ‖ version ‖ 0 ‖ wasm-hash`.
fn digest(manifest: &Manifest, wasm_sha256: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(manifest.id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(manifest.version.as_bytes());
    msg.push(0);
    msg.extend_from_slice(wasm_sha256);
    msg
}

/// Hash module bytes.
pub fn hash_module(wasm: &[u8]) -> [u8; 32] {
    Sha256::digest(wasm).into()
}

/// Sign a package: a publisher hashes `wasm`, then signs the digest.
pub fn sign_package(signing_key: &SigningKey, manifest: Manifest, wasm: &[u8]) -> SignedPackage {
    let wasm_sha256 = hash_module(wasm);
    let signature = signing_key.sign(&digest(&manifest, &wasm_sha256));
    SignedPackage {
        manifest,
        wasm_sha256,
        publisher: signing_key.verifying_key().to_bytes().to_vec(),
        signature: signature.to_bytes().to_vec(),
    }
}

impl SignedPackage {
    /// Verify the signature against `trusted` publishers, and confirm `wasm`
    /// matches the signed hash. On success the verified [`Manifest`] is returned.
    pub fn verify(
        &self,
        wasm: &[u8],
        trusted: &TrustedPublishers,
    ) -> Result<Manifest, MarketError> {
        if !trusted.contains(&self.publisher) {
            return Err(MarketError::UntrustedPublisher);
        }
        if hash_module(wasm) != self.wasm_sha256 {
            return Err(MarketError::HashMismatch);
        }

        let key_bytes: [u8; 32] = self
            .publisher
            .as_slice()
            .try_into()
            .map_err(|_| MarketError::Malformed)?;
        let verifying_key =
            VerifyingKey::from_bytes(&key_bytes).map_err(|_| MarketError::Malformed)?;
        let sig_bytes: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| MarketError::Malformed)?;
        let signature = Signature::from_bytes(&sig_bytes);

        verifying_key
            .verify(&digest(&self.manifest, &self.wasm_sha256), &signature)
            .map_err(|_| MarketError::InvalidSignature)?;
        Ok(self.manifest.clone())
    }
}

/// The set of publisher public keys the host trusts.
#[derive(Debug, Default)]
pub struct TrustedPublishers {
    keys: HashSet<Vec<u8>>,
}

impl TrustedPublishers {
    /// An empty trust set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust a publisher by their verifying key.
    pub fn trust(&mut self, verifying_key: &VerifyingKey) -> &mut Self {
        self.keys.insert(verifying_key.to_bytes().to_vec());
        self
    }

    /// Whether `key_bytes` belongs to a trusted publisher.
    pub fn contains(&self, key_bytes: &[u8]) -> bool {
        self.keys.contains(key_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;

    fn manifest() -> Manifest {
        Manifest {
            id: "com.example.fmt".into(),
            name: "Formatter".into(),
            version: "1.2.0".into(),
            capabilities: vec![Capability::ReadFiles, Capability::WriteFiles],
        }
    }

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn trusted_signed_package_verifies() {
        let publisher = key(7);
        let wasm = b"\x00asm\x01\x00\x00\x00fake module bytes";
        let package = sign_package(&publisher, manifest(), wasm);

        let mut trusted = TrustedPublishers::new();
        trusted.trust(&publisher.verifying_key());

        let verified = package.verify(wasm, &trusted).unwrap();
        assert_eq!(verified.id, "com.example.fmt");
    }

    #[test]
    fn untrusted_publisher_is_rejected() {
        let publisher = key(7);
        let wasm = b"module";
        let package = sign_package(&publisher, manifest(), wasm);

        // A trust set that knows a *different* key.
        let mut trusted = TrustedPublishers::new();
        trusted.trust(&key(9).verifying_key());

        assert!(matches!(
            package.verify(wasm, &trusted),
            Err(MarketError::UntrustedPublisher)
        ));
    }

    #[test]
    fn tampered_module_is_rejected() {
        let publisher = key(7);
        let package = sign_package(&publisher, manifest(), b"original module");
        let mut trusted = TrustedPublishers::new();
        trusted.trust(&publisher.verifying_key());

        // Install a different module than was signed.
        assert!(matches!(
            package.verify(b"malicious module", &trusted),
            Err(MarketError::HashMismatch)
        ));
    }

    #[test]
    fn forged_signature_is_rejected() {
        let publisher = key(7);
        let wasm = b"module";
        let mut package = sign_package(&publisher, manifest(), wasm);
        // Corrupt the signature.
        package.signature[0] ^= 0xff;

        let mut trusted = TrustedPublishers::new();
        trusted.trust(&publisher.verifying_key());
        assert!(matches!(
            package.verify(wasm, &trusted),
            Err(MarketError::InvalidSignature)
        ));
    }
}
