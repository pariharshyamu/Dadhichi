//! A pluggable **state backend** — the store behind the agent's virtual
//! filesystem.
//!
//! Deep agents offload context to files rather than carrying everything in the
//! prompt: they write intermediate results, plans, and notes to a path and read
//! them back later. A [`StateStore`] is that path→content store, behind one
//! trait so the backing can vary:
//!
//! - [`MemStore`] — an in-memory scratchpad, ephemeral and process-local.
//! - [`WorkspaceStore`] — real files under a [`PathJail`]-confined root, so a
//!   write can never escape the workspace (the **sandbox**).
//! - [`OverlayStore`] — a child layer over a parent, where the child's writes
//!   and deletes are isolated; used to give a delegate a scratch space that
//!   doesn't leak back into the caller's state.
//!
//! The store is deliberately synchronous and `String`-valued: it is a simple
//! key/value surface the [`fs`](crate::fs) tools wrap, not a full VFS.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

/// An error from a [`StateStore`] operation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    /// No entry exists at the requested path.
    #[error("no such path: {0}")]
    NotFound(String),
    /// The path escaped the sandbox, or the operation isn't allowed.
    #[error("denied: {0}")]
    Denied(String),
    /// An underlying I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// A path→content store: the backend for the agent's virtual filesystem.
pub trait StateStore: Send + Sync + std::fmt::Debug {
    /// Read the content at `path`.
    fn read(&self, path: &str) -> Result<String, StateError>;
    /// Write `content` at `path`, creating or replacing it.
    fn write(&self, path: &str, content: &str) -> Result<(), StateError>;
    /// List the paths under `prefix` (all paths when `prefix` is empty), sorted.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StateError>;
    /// Delete `path`, returning whether an entry was removed.
    fn delete(&self, path: &str) -> Result<bool, StateError>;
}

/// Normalise `rel` to a clean, forward-slashed key with no `.`/`..` components
/// and no leading slash. Returns [`StateError::Denied`] if it would traverse
/// above the root. Shared by every store so keys are consistent.
fn normalise_key(rel: &str) -> Result<String, StateError> {
    let mut parts: Vec<String> = Vec::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(StateError::Denied(format!("path escapes root: {rel}")));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(StateError::Denied(format!(
                    "absolute paths not allowed: {rel}"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(StateError::Denied(format!("empty path: {rel}")));
    }
    Ok(parts.join("/"))
}

/// Confines a relative path to a root directory, rejecting any traversal that
/// would escape it — the filesystem half of the agent sandbox.
#[derive(Debug, Clone)]
pub struct PathJail {
    root: PathBuf,
}

impl PathJail {
    /// Jail paths to `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The jail's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `rel` to an absolute path guaranteed to sit under the root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, StateError> {
        let key = normalise_key(rel)?;
        let resolved = self.root.join(&key);
        // Belt and braces: the normalised key can't traverse up, but confirm.
        if !resolved.starts_with(&self.root) {
            return Err(StateError::Denied(format!("path escapes sandbox: {rel}")));
        }
        Ok(resolved)
    }
}

/// An in-memory scratchpad store — ephemeral, process-local context offloading.
#[derive(Debug, Default)]
pub struct MemStore {
    files: RwLock<BTreeMap<String, String>>,
}

impl MemStore {
    /// An empty scratchpad.
    pub fn new() -> Self {
        Self::default()
    }

    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, String>> {
        self.files.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, String>> {
        self.files.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl StateStore for MemStore {
    fn read(&self, path: &str) -> Result<String, StateError> {
        let key = normalise_key(path)?;
        self.read_lock()
            .get(&key)
            .cloned()
            .ok_or(StateError::NotFound(key))
    }

    fn write(&self, path: &str, content: &str) -> Result<(), StateError> {
        let key = normalise_key(path)?;
        self.write_lock().insert(key, content.to_string());
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StateError> {
        let files = self.read_lock();
        Ok(files
            .keys()
            .filter(|k| prefix.is_empty() || k.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn delete(&self, path: &str) -> Result<bool, StateError> {
        let key = normalise_key(path)?;
        Ok(self.write_lock().remove(&key).is_some())
    }
}

/// A store backed by real files under a [`PathJail`] — the sandboxed workspace
/// filesystem an agent actually reads and writes.
#[derive(Debug)]
pub struct WorkspaceStore {
    jail: PathJail,
}

impl WorkspaceStore {
    /// Confine reads and writes to `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            jail: PathJail::new(root),
        }
    }

    /// The sandbox root.
    pub fn root(&self) -> &Path {
        self.jail.root()
    }

    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip the usual heavy/hidden dirs so a listing stays useful.
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == ".git" || name == "target" || name == "node_modules" {
                    continue;
                }
                Self::walk(&path, root, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

impl StateStore for WorkspaceStore {
    fn read(&self, path: &str) -> Result<String, StateError> {
        let resolved = self.jail.resolve(path)?;
        std::fs::read_to_string(&resolved).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StateError::NotFound(path.to_string()),
            _ => StateError::Io(e.to_string()),
        })
    }

    fn write(&self, path: &str, content: &str) -> Result<(), StateError> {
        let resolved = self.jail.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StateError::Io(e.to_string()))?;
        }
        std::fs::write(&resolved, content).map_err(|e| StateError::Io(e.to_string()))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StateError> {
        let mut out = Vec::new();
        Self::walk(self.jail.root(), self.jail.root(), &mut out);
        out.retain(|p| prefix.is_empty() || p.starts_with(prefix));
        out.sort();
        Ok(out)
    }

    fn delete(&self, path: &str) -> Result<bool, StateError> {
        let resolved = self.jail.resolve(path)?;
        match std::fs::remove_file(&resolved) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StateError::Io(e.to_string())),
        }
    }
}

/// A copy-on-write layer over a parent store. Writes and deletes land only in
/// the overlay, so a delegate can scribble freely without its changes leaking
/// back into the caller's state — the filesystem analogue of an isolated
/// context. Reads fall through to the parent for anything the child hasn't
/// touched.
#[derive(Debug)]
pub struct OverlayStore {
    base: Arc<dyn StateStore>,
    overlay: MemStore,
    deleted: RwLock<HashSet<String>>,
}

impl OverlayStore {
    /// Layer a fresh, empty overlay over `base`.
    pub fn new(base: Arc<dyn StateStore>) -> Self {
        Self {
            base,
            overlay: MemStore::new(),
            deleted: RwLock::new(HashSet::new()),
        }
    }

    fn deleted_lock(&self) -> std::sync::RwLockReadGuard<'_, HashSet<String>> {
        self.deleted.read().unwrap_or_else(|e| e.into_inner())
    }
}

impl StateStore for OverlayStore {
    fn read(&self, path: &str) -> Result<String, StateError> {
        let key = normalise_key(path)?;
        if self.deleted_lock().contains(&key) {
            return Err(StateError::NotFound(key));
        }
        match self.overlay.read(path) {
            Ok(content) => Ok(content),
            Err(StateError::NotFound(_)) => self.base.read(path),
            Err(other) => Err(other),
        }
    }

    fn write(&self, path: &str, content: &str) -> Result<(), StateError> {
        let key = normalise_key(path)?;
        self.deleted
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.overlay.write(path, content)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StateError> {
        let deleted = self.deleted_lock();
        let mut set: std::collections::BTreeSet<String> = self
            .base
            .list(prefix)?
            .into_iter()
            .filter(|k| !deleted.contains(k))
            .collect();
        set.extend(self.overlay.list(prefix)?);
        Ok(set.into_iter().collect())
    }

    fn delete(&self, path: &str) -> Result<bool, StateError> {
        let key = normalise_key(path)?;
        let in_overlay = self.overlay.delete(path)?;
        let in_base = self.base.read(path).is_ok();
        if in_base {
            self.deleted
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
        }
        Ok(in_overlay || in_base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn normalise_rejects_traversal_and_absolute() {
        assert_eq!(normalise_key("a/b/../c").unwrap(), "a/c");
        assert!(matches!(
            normalise_key("../etc"),
            Err(StateError::Denied(_))
        ));
        assert!(matches!(
            normalise_key("/etc/passwd"),
            Err(StateError::Denied(_))
        ));
    }

    #[test]
    fn mem_store_round_trips_and_lists() {
        let store = MemStore::new();
        store.write("notes/todo.md", "buy milk").unwrap();
        store.write("notes/done.md", "shipped").unwrap();
        store.write("src/main.rs", "fn main() {}").unwrap();
        assert_eq!(store.read("notes/todo.md").unwrap(), "buy milk");
        assert_eq!(
            store.list("notes/").unwrap(),
            vec!["notes/done.md", "notes/todo.md"]
        );
        assert!(store.delete("notes/todo.md").unwrap());
        assert!(matches!(
            store.read("notes/todo.md"),
            Err(StateError::NotFound(_))
        ));
    }

    #[test]
    fn workspace_store_confines_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        store.write("sub/file.txt", "hello").unwrap();
        assert_eq!(store.read("sub/file.txt").unwrap(), "hello");
        // The write really landed under the root.
        assert!(dir.path().join("sub/file.txt").exists());
        // A traversal escape is denied, not followed.
        assert!(matches!(
            store.write("../escape.txt", "nope"),
            Err(StateError::Denied(_))
        ));
        assert!(matches!(
            store.read("../../etc/passwd"),
            Err(StateError::Denied(_))
        ));
    }

    #[test]
    fn overlay_isolates_child_writes_from_the_base() {
        let base: Arc<dyn StateStore> = Arc::new(MemStore::new());
        base.write("shared.txt", "from parent").unwrap();
        let overlay = OverlayStore::new(base.clone());

        // Reads fall through to the base.
        assert_eq!(overlay.read("shared.txt").unwrap(), "from parent");
        // A child write is visible to the child but not the base.
        overlay.write("scratch.txt", "child only").unwrap();
        overlay.write("shared.txt", "child override").unwrap();
        assert_eq!(overlay.read("scratch.txt").unwrap(), "child only");
        assert_eq!(overlay.read("shared.txt").unwrap(), "child override");
        assert_eq!(base.read("shared.txt").unwrap(), "from parent");
        assert!(matches!(
            base.read("scratch.txt"),
            Err(StateError::NotFound(_))
        ));

        // A child delete hides a base entry from the child only.
        assert!(overlay.delete("shared.txt").unwrap());
        assert!(matches!(
            overlay.read("shared.txt"),
            Err(StateError::NotFound(_))
        ));
        assert_eq!(base.read("shared.txt").unwrap(), "from parent");
    }
}
