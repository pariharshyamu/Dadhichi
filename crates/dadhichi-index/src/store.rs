//! Persistent symbol storage.
//!
//! The [`SymbolStore`] trait is the query surface code intelligence is built on;
//! [`SqliteSymbolStore`] is the durable backend (SQLite, via `rusqlite`). It is
//! **incremental** — [`replace_file`](SymbolStore::replace_file) swaps only one
//! file's rows — so a single save re-indexes without rewriting the database.

use dadhichi_workspace::{Symbol, SymbolKind};
use std::path::Path;
use std::sync::Mutex;
use thiserror::Error;

/// Errors from the symbol store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The underlying SQLite layer failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A persistent, queryable store of definition symbols.
pub trait SymbolStore: Send + Sync {
    /// Replace every symbol previously recorded for `file` with `symbols`.
    fn replace_file(&self, file: &Path, symbols: &[Symbol]) -> Result<(), StoreError>;

    /// Resolve `name` to its definitions (powers go-to-definition).
    fn definitions(&self, name: &str) -> Result<Vec<Symbol>, StoreError>;

    /// Total number of stored symbols.
    fn symbol_count(&self) -> Result<usize, StoreError>;
}

/// A SQLite-backed [`SymbolStore`].
#[derive(Debug)]
pub struct SqliteSymbolStore {
    // rusqlite's Connection is not Sync; a Mutex makes the store shareable.
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteSymbolStore {
    /// Open an in-memory store (ideal for tests and ephemeral sessions).
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(rusqlite::Connection::open_in_memory()?)
    }

    /// Open (creating if absent) a store backed by a file on disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_connection(rusqlite::Connection::open(path)?)
    }

    fn from_connection(conn: rusqlite::Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS symbols (
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 file TEXT NOT NULL,
                 line INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
             CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

impl SymbolStore for SqliteSymbolStore {
    fn replace_file(&self, file: &Path, symbols: &[Symbol]) -> Result<(), StoreError> {
        let file_str = file.to_string_lossy();
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM symbols WHERE file = ?1", [file_str.as_ref()])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO symbols (name, kind, file, line) VALUES (?1, ?2, ?3, ?4)")?;
            for sym in symbols {
                stmt.execute(rusqlite::params![
                    sym.name,
                    kind_to_str(sym.kind),
                    sym.file.to_string_lossy(),
                    sym.line,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn definitions(&self, name: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT name, kind, file, line FROM symbols WHERE name = ?1")?;
        let rows = stmt.query_map([name], |row| {
            let name: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let file: String = row.get(2)?;
            let line: u32 = row.get(3)?;
            Ok(Symbol {
                name,
                kind: str_to_kind(&kind),
                file: file.into(),
                line,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn symbol_count(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

fn kind_to_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Function => "function",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
    }
}

fn str_to_kind(s: &str) -> SymbolKind {
    match s {
        "module" => SymbolKind::Module,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "trait" => SymbolKind::Trait,
        "constant" => SymbolKind::Constant,
        "variable" => SymbolKind::Variable,
        _ => SymbolKind::Function,
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
    fn stores_and_queries_symbols() {
        let store = SqliteSymbolStore::in_memory().unwrap();
        store
            .replace_file(Path::new("a.rs"), &[sym("run", "a.rs", 3)])
            .unwrap();

        let defs = store.definitions("run").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].line, 3);
        assert_eq!(store.symbol_count().unwrap(), 1);
    }

    #[test]
    fn replace_file_is_incremental() {
        let store = SqliteSymbolStore::in_memory().unwrap();
        store
            .replace_file(Path::new("a.rs"), &[sym("old", "a.rs", 1)])
            .unwrap();
        store
            .replace_file(Path::new("b.rs"), &[sym("keep", "b.rs", 1)])
            .unwrap();
        // Re-indexing a.rs must not disturb b.rs.
        store
            .replace_file(Path::new("a.rs"), &[sym("new", "a.rs", 1)])
            .unwrap();

        assert!(store.definitions("old").unwrap().is_empty());
        assert_eq!(store.definitions("new").unwrap().len(), 1);
        assert_eq!(store.definitions("keep").unwrap().len(), 1);
        assert_eq!(store.symbol_count().unwrap(), 2);
    }
}
