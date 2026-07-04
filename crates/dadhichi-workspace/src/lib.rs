//! # dadhichi-workspace
//!
//! The workspace model: a set of roots (folders, monorepos, git worktrees) and
//! an incremental symbol index over them. Code intelligence — go-to-definition,
//! references, the call graph, semantic search — is built on top of the
//! [`SymbolIndex`] this crate maintains.
//!
//! The index is deliberately storage-agnostic: the in-memory implementation
//! here is the reference, and production backends (SQLite for symbols, LanceDB
//! for embeddings) implement the same query surface.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors from workspace operations.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// A root path was added that does not exist or is not a directory.
    #[error("invalid workspace root: {0}")]
    InvalidRoot(PathBuf),
    /// A referenced file is not tracked by the workspace.
    #[error("file not tracked: {0}")]
    UntrackedFile(PathBuf),
}

/// The kind of a symbol, mirroring LSP `SymbolKind` at a coarse grain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A module or namespace.
    Module,
    /// A function or method.
    Function,
    /// A struct, class, or record.
    Struct,
    /// An enum or sum type.
    Enum,
    /// A trait or interface.
    Trait,
    /// A constant or static.
    Constant,
    /// A field or variable.
    Variable,
}

/// A named symbol located at a position in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// The symbol's identifier.
    pub name: String,
    /// What kind of symbol it is.
    pub kind: SymbolKind,
    /// The file the symbol is defined in.
    pub file: PathBuf,
    /// 1-based line number of the definition.
    pub line: u32,
}

/// A workspace root: one folder that participates in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    /// The absolute path of the root.
    pub path: PathBuf,
    /// A display name (usually the final path component).
    pub name: String,
}

/// An in-memory symbol index supporting the core code-intelligence queries.
///
/// The design is incremental: [`index_file`](Self::index_file) replaces only
/// the entries for one file, so a file-watcher can re-index a single save
/// without touching the rest of the graph.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    /// symbol name -> definitions (a name may resolve to several symbols).
    by_name: HashMap<String, Vec<Symbol>>,
    /// file -> symbols defined in it, for incremental replacement.
    by_file: HashMap<PathBuf, Vec<Symbol>>,
}

impl SymbolIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all symbols for `file` with `symbols`.
    ///
    /// Old entries for the file are removed first, keeping the name index
    /// consistent after an edit.
    pub fn index_file(&mut self, file: impl AsRef<Path>, symbols: Vec<Symbol>) {
        let file = file.as_ref().to_path_buf();
        self.remove_file(&file);
        for sym in &symbols {
            self.by_name
                .entry(sym.name.clone())
                .or_default()
                .push(sym.clone());
        }
        self.by_file.insert(file, symbols);
    }

    /// Drop every symbol defined in `file` (e.g. when it is deleted).
    pub fn remove_file(&mut self, file: impl AsRef<Path>) {
        let file = file.as_ref();
        if let Some(old) = self.by_file.remove(file) {
            for sym in old {
                if let Some(defs) = self.by_name.get_mut(&sym.name) {
                    defs.retain(|s| s.file != sym.file || s.line != sym.line);
                    if defs.is_empty() {
                        self.by_name.remove(&sym.name);
                    }
                }
            }
        }
    }

    /// Resolve `name` to its definitions (powers go-to-definition).
    pub fn definitions(&self, name: &str) -> &[Symbol] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.by_name.values().map(Vec::len).sum()
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.by_file.len()
    }
}

/// A multi-root workspace with an attached symbol index.
#[derive(Debug, Default)]
pub struct Workspace {
    roots: Vec<Root>,
    index: SymbolIndex,
}

impl Workspace {
    /// Create an empty workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a folder as a workspace root.
    pub fn add_root(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        self.roots.push(Root { path, name });
    }

    /// The workspace roots.
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Immutable access to the symbol index.
    pub fn index(&self) -> &SymbolIndex {
        &self.index
    }

    /// Mutable access to the symbol index (for the indexer/file-watcher).
    pub fn index_mut(&mut self) -> &mut SymbolIndex {
        &mut self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, file: &str, line: u32) -> Symbol {
        Symbol {
            name: name.into(),
            kind: SymbolKind::Function,
            file: file.into(),
            line,
        }
    }

    #[test]
    fn indexing_and_lookup() {
        let mut ws = Workspace::new();
        ws.add_root("/project");
        ws.index_mut()
            .index_file("a.rs", vec![sym("run", "a.rs", 10), sym("init", "a.rs", 2)]);

        assert_eq!(ws.index().definitions("run").len(), 1);
        assert_eq!(ws.index().definitions("run")[0].line, 10);
        assert_eq!(ws.index().symbol_count(), 2);
    }

    #[test]
    fn reindex_replaces_previous_symbols() {
        let mut index = SymbolIndex::new();
        index.index_file("a.rs", vec![sym("old", "a.rs", 1)]);
        index.index_file("a.rs", vec![sym("new", "a.rs", 1)]);

        assert!(index.definitions("old").is_empty());
        assert_eq!(index.definitions("new").len(), 1);
        assert_eq!(index.file_count(), 1);
    }

    #[test]
    fn removing_a_file_clears_its_symbols() {
        let mut index = SymbolIndex::new();
        index.index_file("a.rs", vec![sym("gone", "a.rs", 1)]);
        index.remove_file("a.rs");
        assert!(index.definitions("gone").is_empty());
        assert_eq!(index.symbol_count(), 0);
    }
}
