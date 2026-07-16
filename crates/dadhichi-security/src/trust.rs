//! Folder trust: one gate for running a repository's own automation.
//!
//! Declarative permission *rules* in a project are safe to read — they only
//! ever restrict. But executable extensions a repo ships (lifecycle hooks,
//! repo-local MCP/LSP servers) are code, so a freshly-cloned untrusted repo
//! must not run them until the user vouches for the folder. [`TrustStore`] is
//! that gate: a single persisted allow-list of trusted directories that
//! **cascades to subdirectories**, so trusting a repo root trusts its whole
//! tree.
//!
//! The store is intentionally simple and JSON-backed (no new dependency): a
//! flat list of absolute paths under `~/.dadhichi/trusted_folders.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A persisted set of trusted directories.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustData {
    trusted: Vec<PathBuf>,
}

impl TrustStore {
    /// A store backed by `path` (the JSON file need not exist yet).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default store at `~/.dadhichi/trusted_folders.json`, honouring a
    /// `DADHICHI_HOME` override. `None` when no home directory is known.
    pub fn at_home() -> Option<Self> {
        let home = std::env::var_os("DADHICHI_HOME")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)?;
        Some(Self::new(
            home.join(".dadhichi").join("trusted_folders.json"),
        ))
    }

    /// Whether `dir` is trusted — either it is listed, or it lies inside a
    /// listed directory (trust cascades downward).
    pub fn is_trusted(&self, dir: &Path) -> bool {
        let dir = normalize(dir);
        self.load()
            .trusted
            .iter()
            .any(|t| dir == *t || dir.starts_with(t))
    }

    /// Trust `dir` (idempotent). Persists immediately.
    pub fn trust(&self, dir: &Path) -> std::io::Result<()> {
        let dir = normalize(dir);
        let mut data = self.load();
        if !data.trusted.contains(&dir) {
            data.trusted.push(dir);
            self.save(&data)?;
        }
        Ok(())
    }

    /// Revoke trust for `dir` exactly (does not touch parent/child entries).
    /// Returns whether an entry was removed.
    pub fn untrust(&self, dir: &Path) -> std::io::Result<bool> {
        let dir = normalize(dir);
        let mut data = self.load();
        let before = data.trusted.len();
        data.trusted.retain(|t| *t != dir);
        let removed = data.trusted.len() != before;
        if removed {
            self.save(&data)?;
        }
        Ok(removed)
    }

    /// The trusted directories, as stored.
    pub fn list(&self) -> Vec<PathBuf> {
        self.load().trusted
    }

    fn load(&self) -> TrustData {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, data: &TrustData) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(data)?;
        std::fs::write(&self.path, json)
    }
}

/// Canonicalize a path if it exists on disk; otherwise fall back to the path as
/// given, so trust decisions are stable for directories that don't yet exist.
fn normalize(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::new(dir.path().join("trust.json"));
        assert!(!store.is_trusted(dir.path()));
    }

    #[test]
    fn trust_cascades_to_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::new(dir.path().join("trust.json"));
        let repo = dir.path().join("repo");
        let sub = repo.join("crates").join("thing");
        std::fs::create_dir_all(&sub).unwrap();

        store.trust(&repo).unwrap();
        assert!(store.is_trusted(&repo));
        assert!(store.is_trusted(&sub)); // cascades down
        assert!(!store.is_trusted(dir.path())); // but not up to the parent
    }

    #[test]
    fn trust_is_idempotent_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trust.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let store = TrustStore::new(&file);
        store.trust(&repo).unwrap();
        store.trust(&repo).unwrap(); // idempotent
        assert_eq!(store.list().len(), 1);

        // A fresh store reads the persisted decision.
        let reopened = TrustStore::new(&file);
        assert!(reopened.is_trusted(&repo));
    }

    #[test]
    fn untrust_removes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store = TrustStore::new(dir.path().join("trust.json"));

        store.trust(&repo).unwrap();
        assert!(store.untrust(&repo).unwrap());
        assert!(!store.is_trusted(&repo));
        assert!(!store.untrust(&repo).unwrap()); // nothing left to remove
    }
}
