//! A tamper-evident audit log.
//!
//! Every security-relevant action — a tool invocation, a permission grant, a
//! vault access — is appended as an [`AuditEntry`]. Entries are hash-chained:
//! each carries the SHA-256 of `(previous_hash ‖ entry)`, so altering or
//! removing any past entry breaks the chain and [`AuditLog::verify`] detects it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A single recorded action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Who performed the action (agent, user, plugin id).
    pub actor: String,
    /// What was done (e.g. `tool.invoke`, `vault.get`).
    pub action: String,
    /// The target of the action (e.g. a tool name or file path).
    pub resource: String,
    /// Whether the action was permitted.
    pub allowed: bool,
    /// The chain hash: hex SHA-256 of `prev_hash ‖ canonical(entry)`.
    pub hash: String,
}

/// An append-only, hash-chained log.
#[derive(Debug, Default)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}

impl AuditLog {
    /// Create an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an action, computing and storing its chain hash.
    pub fn record(&mut self, actor: &str, action: &str, resource: &str, allowed: bool) {
        let seq = self.entries.len() as u64;
        let prev = self
            .entries
            .last()
            .map(|e| e.hash.as_str())
            .unwrap_or("genesis");
        let hash = chain_hash(prev, seq, actor, action, resource, allowed);
        self.entries.push(AuditEntry {
            seq,
            actor: actor.to_string(),
            action: action.to_string(),
            resource: resource.to_string(),
            allowed,
            hash,
        });
    }

    /// The recorded entries.
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Recompute the chain and verify no entry has been altered or removed.
    pub fn verify(&self) -> bool {
        let mut prev = "genesis".to_string();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.seq != i as u64 {
                return false;
            }
            let expected = chain_hash(
                &prev,
                entry.seq,
                &entry.actor,
                &entry.action,
                &entry.resource,
                entry.allowed,
            );
            if expected != entry.hash {
                return false;
            }
            prev = entry.hash.clone();
        }
        true
    }
}

fn chain_hash(
    prev: &str,
    seq: u64,
    actor: &str,
    action: &str,
    resource: &str,
    allowed: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev.as_bytes());
    hasher.update(seq.to_le_bytes());
    hasher.update(actor.as_bytes());
    hasher.update([0]);
    hasher.update(action.as_bytes());
    hasher.update([0]);
    hasher.update(resource.as_bytes());
    hasher.update([allowed as u8]);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> AuditLog {
        let mut log = AuditLog::new();
        log.record("code-agent", "tool.invoke", "fs.write", true);
        log.record("code-agent", "vault.get", "openai", false);
        log.record("user", "tool.invoke", "git.commit", true);
        log
    }

    #[test]
    fn intact_log_verifies() {
        let log = populated();
        assert_eq!(log.len(), 3);
        assert!(log.verify());
    }

    #[test]
    fn altering_an_entry_breaks_the_chain() {
        let mut log = populated();
        // Forge a denied action into an allowed one.
        log.entries[1].allowed = true;
        assert!(!log.verify(), "tampering must be detected");
    }

    #[test]
    fn removing_an_entry_breaks_the_chain() {
        let mut log = populated();
        log.entries.remove(1);
        assert!(!log.verify());
    }
}
