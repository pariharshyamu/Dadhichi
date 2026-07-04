//! An encrypted credential vault.
//!
//! Secrets (API keys, tokens) are stored encrypted with ChaCha20-Poly1305 under
//! a key derived from a master passphrase. Ciphertext is authenticated, so
//! tampering is detected on decrypt, and the wrong passphrase simply fails to
//! decrypt. Nonces come from a monotonic counter persisted with the vault, so
//! they never repeat for a given key.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

/// Errors from the vault.
#[derive(Debug, Error)]
pub enum VaultError {
    /// Decryption failed — wrong passphrase or tampered ciphertext.
    #[error("decryption failed (wrong passphrase or corrupted data)")]
    Decryption,
    /// Encryption failed.
    #[error("encryption failed")]
    Encryption,
}

/// A single encrypted entry: its nonce and authenticated ciphertext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sealed {
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

/// The persistable, fully-encrypted contents of a vault. Safe to write to disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultData {
    entries: BTreeMap<String, Sealed>,
    counter: u64,
}

/// An encrypted credential store keyed by a master passphrase.
pub struct Vault {
    cipher: ChaCha20Poly1305,
    data: VaultData,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("entries", &self.data.entries.len())
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// Open an empty vault under `master`.
    pub fn new(master: &str) -> Self {
        Self::with_data(master, VaultData::default())
    }

    /// Reopen a vault from previously persisted [`VaultData`] under `master`.
    pub fn with_data(master: &str, data: VaultData) -> Self {
        let key_bytes = Sha256::digest(master.as_bytes());
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
        Self { cipher, data }
    }

    /// The encrypted contents, for persistence.
    pub fn data(&self) -> &VaultData {
        &self.data
    }

    /// The names of stored secrets (values stay encrypted).
    pub fn names(&self) -> Vec<String> {
        self.data.entries.keys().cloned().collect()
    }

    /// Store `secret` under `name`, encrypting it.
    pub fn put(&mut self, name: &str, secret: &str) -> Result<(), VaultError> {
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[..8].copy_from_slice(&self.data.counter.to_le_bytes());
        self.data.counter += 1;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, secret.as_bytes())
            .map_err(|_| VaultError::Encryption)?;
        self.data.entries.insert(
            name.to_string(),
            Sealed {
                nonce: nonce_bytes,
                ciphertext,
            },
        );
        Ok(())
    }

    /// Retrieve and decrypt the secret under `name`.
    ///
    /// Returns `Ok(None)` if there is no such entry, and
    /// `Err(VaultError::Decryption)` if the passphrase is wrong or the data was
    /// tampered with.
    pub fn get(&self, name: &str) -> Result<Option<String>, VaultError> {
        let Some(sealed) = self.data.entries.get(name) else {
            return Ok(None);
        };
        let nonce = Nonce::from_slice(&sealed.nonce);
        let plaintext = self
            .cipher
            .decrypt(nonce, sealed.ciphertext.as_ref())
            .map_err(|_| VaultError::Decryption)?;
        String::from_utf8(plaintext)
            .map(Some)
            .map_err(|_| VaultError::Decryption)
    }

    /// Remove a secret. Returns whether it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.data.entries.remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_secrets() {
        let mut vault = Vault::new("correct horse battery staple");
        vault.put("openai", "sk-secret-key").unwrap();
        vault.put("github", "ghp_token").unwrap();

        assert_eq!(
            vault.get("openai").unwrap().as_deref(),
            Some("sk-secret-key")
        );
        assert_eq!(vault.get("missing").unwrap(), None);
        assert_eq!(vault.names().len(), 2);
    }

    #[test]
    fn wrong_passphrase_cannot_decrypt() {
        let mut vault = Vault::new("right");
        vault.put("api", "value").unwrap();
        let persisted = vault.data().clone();

        let attacker = Vault::with_data("wrong", persisted);
        assert!(matches!(attacker.get("api"), Err(VaultError::Decryption)));
    }

    #[test]
    fn tampering_is_detected() {
        let mut vault = Vault::new("key");
        vault.put("api", "value").unwrap();
        let mut data = vault.data().clone();
        // Flip a byte of the ciphertext.
        if let Some(sealed) = data.entries.get_mut("api") {
            sealed.ciphertext[0] ^= 0xff;
        }
        let reopened = Vault::with_data("key", data);
        assert!(matches!(reopened.get("api"), Err(VaultError::Decryption)));
    }

    #[test]
    fn nonces_do_not_repeat() {
        let mut vault = Vault::new("key");
        vault.put("a", "x").unwrap();
        vault.put("b", "x").unwrap();
        let nonces: Vec<_> = vault.data().entries.values().map(|s| s.nonce).collect();
        assert_ne!(nonces[0], nonces[1]);
    }
}
