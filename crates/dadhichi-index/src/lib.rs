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

use dadhichi_cache::BlobCache;
use dadhichi_core::{Event, EventBus};
use dadhichi_parse::{LanguageParser, Parsed, RustParser};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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
    cache: Option<Arc<dyn BlobCache>>,
}

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexer")
            .field("has_bus", &self.bus.is_some())
            .field("has_cache", &self.cache.is_some())
            .finish_non_exhaustive()
    }
}

impl Indexer {
    /// Create an indexer writing into `store`, with no event bus attached.
    pub fn new(store: Arc<dyn SymbolStore>) -> Self {
        Self {
            store,
            bus: None,
            cache: None,
        }
    }

    /// Attach a kernel event bus so the indexer emits `symbols.updated` and
    /// `fs.changed` events.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Attach a persistent blob cache. Parse results are memoised by file path
    /// and content hash, so an unchanged file is never re-parsed.
    pub fn with_blob_cache(mut self, cache: Arc<dyn BlobCache>) -> Self {
        self.cache = Some(cache);
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

    /// Analyse `source`, consulting the blob cache first when present.
    fn analyze(&self, parser: &dyn LanguageParser, path: &Path, source: &str) -> Parsed {
        let Some(cache) = &self.cache else {
            return parser.parse_all(source, path);
        };
        let key = cache_key(path, source);
        if let Ok(Some(bytes)) = cache.get(key.as_bytes())
            && let Ok(parsed) = serde_json::from_slice::<Parsed>(&bytes)
        {
            return parsed;
        }
        let parsed = parser.parse_all(source, path);
        if let Ok(bytes) = serde_json::to_vec(&parsed) {
            let _ = cache.put(key.as_bytes(), &bytes);
        }
        parsed
    }

    /// Parse `source` for `path`, replace that file's symbols and references in
    /// the store, and emit `symbols.updated`. Returns the number of symbols
    /// indexed (0 if the language is unsupported).
    pub fn index_source(&self, path: impl AsRef<Path>, source: &str) -> Result<usize, IndexError> {
        let path = path.as_ref();
        let Some(parser) = Self::parser_for(path) else {
            return Ok(0);
        };
        let parsed = self.analyze(&*parser, path, source);
        self.store.replace_file(path, &parsed.symbols)?;
        self.store
            .replace_file_references(path, &parsed.references)?;
        self.emit(
            "symbols.updated",
            serde_json::json!({
                "file": path.to_string_lossy(),
                "count": parsed.symbols.len(),
                "references": parsed.references.len(),
            }),
        );
        Ok(parsed.symbols.len())
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

/// A content-addressed cache key: the file path plus a hash of its contents, so
/// editing a file invalidates its cached parse while a byte-identical file hits.
fn cache_key(path: &Path, source: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    format!("parse:{}:{:x}", path.to_string_lossy(), hasher.finish())
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

    #[test]
    fn indexes_the_call_graph() {
        let (indexer, store) = indexer();
        indexer
            .index_source("a.rs", "fn helper() {}\nfn main() { helper(); }")
            .unwrap();
        // `main` calls `helper`.
        let callers = store.callers_of("helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].from.as_deref(), Some("main"));
    }

    /// An in-memory `BlobCache` that counts writes, to prove cache hits skip
    /// re-parsing.
    #[derive(Default)]
    struct CountingCache {
        map: std::sync::Mutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
        puts: std::sync::atomic::AtomicU64,
    }

    impl BlobCache for CountingCache {
        fn put(&self, key: &[u8], value: &[u8]) -> Result<(), dadhichi_cache::CacheError> {
            self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.map
                .lock()
                .unwrap()
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, dadhichi_cache::CacheError> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }
        fn delete(&self, key: &[u8]) -> Result<(), dadhichi_cache::CacheError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn blob_cache_avoids_reparsing_unchanged_files() {
        let store = Arc::new(SqliteSymbolStore::in_memory().unwrap());
        let cache = Arc::new(CountingCache::default());
        let indexer = Indexer::new(store).with_blob_cache(cache.clone());

        let src = "fn main() {}";
        indexer.index_source("a.rs", src).unwrap(); // miss → parse + put
        indexer.index_source("a.rs", src).unwrap(); // hit  → no put

        assert_eq!(cache.puts.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
