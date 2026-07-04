//! # dadhichi-index
//!
//! The **incremental indexing service**. It ties the tree-sitter parser
//! ([`dadhichi-parse`](dadhichi_parse)) to the persistent symbol store and the
//! kernel event bus, realising the Phase 2 data flow:
//!
//! ```text
//! notify watcher ─▶ fs.changed ─▶ parse ─▶ store.replace_file ─▶ symbols.updated
//! ```
//!
//! Indexing is incremental at the file grain: a single save re-parses and
//! re-stores just that file, then emits `symbols.updated` so code-intelligence
//! consumers refresh.
//!
//! ```
//! use dadhichi_index::Indexer;
//! use dadhichi_index::store::SqliteSymbolStore;
//! use std::sync::Arc;
//!
//! let store = Arc::new(SqliteSymbolStore::in_memory().unwrap());
//! let indexer = Indexer::new(store.clone());
//! let n = indexer.index_source("lib.rs", "fn main() {}\nstruct S;").unwrap();
//! assert_eq!(n, 2);
//! ```

pub mod store;

use dadhichi_core::{Event, EventBus};
use dadhichi_parse::{LanguageParser, RustParser};
use std::path::Path;
use std::sync::Arc;
use store::{StoreError, SymbolStore};
use thiserror::Error;

/// Errors from the indexer.
#[derive(Debug, Error)]
pub enum IndexError {
    /// Reading a source file failed.
    #[error("io error for {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The symbol store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The file watcher failed.
    #[error("watch error: {0}")]
    Watch(String),
}

/// Indexes source files into a [`SymbolStore`], emitting progress on the bus.
///
/// Cheap to clone (it holds `Arc`/`EventBus` handles), so the same indexer can
/// be shared between the initial workspace scan and the live file watcher.
#[derive(Clone)]
pub struct Indexer {
    store: Arc<dyn SymbolStore>,
    bus: Option<EventBus>,
}

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexer")
            .field("has_bus", &self.bus.is_some())
            .finish_non_exhaustive()
    }
}

impl Indexer {
    /// Create an indexer writing into `store`, with no event bus attached.
    pub fn new(store: Arc<dyn SymbolStore>) -> Self {
        Self { store, bus: None }
    }

    /// Attach a kernel event bus so the indexer emits `symbols.updated` and
    /// `fs.changed` events.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// The parser for `path`, chosen by file extension. Returns `None` for
    /// unsupported languages (which are simply skipped).
    fn parser_for(path: &Path) -> Option<Box<dyn LanguageParser>> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Box::new(RustParser::new())),
            _ => None,
        }
    }

    /// Parse `source` for `path`, replace that file's symbols in the store, and
    /// emit `symbols.updated`. Returns the number of symbols indexed (0 if the
    /// language is unsupported).
    pub fn index_source(&self, path: impl AsRef<Path>, source: &str) -> Result<usize, IndexError> {
        let path = path.as_ref();
        let Some(parser) = Self::parser_for(path) else {
            return Ok(0);
        };
        let symbols = parser.parse(source, path);
        self.store.replace_file(path, &symbols)?;
        self.emit(
            "symbols.updated",
            serde_json::json!({ "file": path.to_string_lossy(), "count": symbols.len() }),
        );
        Ok(symbols.len())
    }

    /// Read `path` from disk and index it.
    pub fn index_file(&self, path: impl AsRef<Path>) -> Result<usize, IndexError> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path).map_err(|source| IndexError::Io {
            path: path.to_string_lossy().into_owned(),
            source,
        })?;
        self.index_source(path, &source)
    }

    /// Recursively index every supported source file under `dir`, returning the
    /// total number of symbols found. Used for the initial workspace scan.
    pub fn index_dir(&self, dir: impl AsRef<Path>) -> Result<usize, IndexError> {
        let mut total = 0;
        let mut stack = vec![dir.as_ref().to_path_buf()];
        while let Some(current) = stack.pop() {
            let entries = std::fs::read_dir(&current).map_err(|source| IndexError::Io {
                path: current.to_string_lossy().into_owned(),
                source,
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if Self::parser_for(&path).is_some() {
                    total += self.index_file(&path)?;
                }
            }
        }
        Ok(total)
    }

    /// Start watching `dir` recursively, re-indexing supported files as they
    /// change. The returned [`WatchGuard`] must be kept alive; dropping it stops
    /// watching.
    pub fn watch(&self, dir: impl AsRef<Path>) -> Result<WatchGuard, IndexError> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let indexer = self.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                return;
            }
            for path in event.paths {
                if Indexer::parser_for(&path).is_none() {
                    continue;
                }
                indexer.emit(
                    "fs.changed",
                    serde_json::json!({ "path": path.to_string_lossy() }),
                );
                if let Err(err) = indexer.index_file(&path) {
                    tracing::warn!(%err, "re-index after change failed");
                }
            }
        })
        .map_err(|e| IndexError::Watch(e.to_string()))?;

        watcher
            .watch(dir.as_ref(), RecursiveMode::Recursive)
            .map_err(|e| IndexError::Watch(e.to_string()))?;
        Ok(WatchGuard { _watcher: watcher })
    }

    fn emit(&self, topic: &str, payload: serde_json::Value) {
        if let Some(bus) = &self.bus {
            bus.publish(Event::new(topic, payload));
        }
    }
}

/// Keeps a filesystem watch alive; dropping it stops the watch.
pub struct WatchGuard {
    _watcher: notify::RecommendedWatcher,
}

impl std::fmt::Debug for WatchGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuard").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::SqliteSymbolStore;

    fn indexer() -> (Indexer, Arc<SqliteSymbolStore>) {
        let store = Arc::new(SqliteSymbolStore::in_memory().unwrap());
        (Indexer::new(store.clone()), store)
    }

    #[test]
    fn indexes_source_into_store() {
        let (indexer, store) = indexer();
        let n = indexer
            .index_source("lib.rs", "fn main() {}\nstruct S;")
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.definitions("main").unwrap().len(), 1);
        assert_eq!(store.definitions("S").unwrap().len(), 1);
    }

    #[test]
    fn unsupported_extension_is_skipped() {
        let (indexer, store) = indexer();
        assert_eq!(
            indexer.index_source("notes.txt", "fn main() {}").unwrap(),
            0
        );
        assert_eq!(store.symbol_count().unwrap(), 0);
    }

    #[test]
    fn index_dir_walks_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.rs"), "struct B;").unwrap();
        std::fs::write(dir.path().join("readme.md"), "# ignore me").unwrap();

        let (indexer, store) = indexer();
        let total = indexer.index_dir(dir.path()).unwrap();
        assert_eq!(total, 2);
        assert_eq!(store.definitions("a").unwrap().len(), 1);
        assert_eq!(store.definitions("B").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn emits_symbols_updated_event() {
        let store = Arc::new(SqliteSymbolStore::in_memory().unwrap());
        let bus = EventBus::new();
        let mut sub = bus.subscribe_topic("symbols.updated");
        let indexer = Indexer::new(store).with_event_bus(bus);

        indexer.index_source("x.rs", "fn go() {}").unwrap();

        let event = sub.recv().await.expect("event delivered");
        assert_eq!(event.payload["count"], 1);
        assert_eq!(event.payload["file"], "x.rs");
    }
}
