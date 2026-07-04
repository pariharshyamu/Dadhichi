//! # dadhichi-cache
//!
//! A **persistent, high-throughput blob cache** backing the indexer's parsed
//! ASTs and, later, incremental build artifacts. The [`BlobCache`] trait is the
//! query surface; [`RocksBlobCache`] is the durable [RocksDB] backend.
//!
//! The cache is content-addressed by the caller: keys are arbitrary bytes
//! (typically `path + content-hash`), so an unchanged file's parse result is
//! served from disk instead of being recomputed.
//!
//! [RocksDB]: https://rocksdb.org

use std::path::Path;
use thiserror::Error;

/// Errors from the blob cache.
#[derive(Debug, Error)]
pub enum CacheError {
    /// The underlying storage engine failed.
    #[error("cache backend error: {0}")]
    Backend(String),
}

/// A persistent key → bytes store.
pub trait BlobCache: Send + Sync {
    /// Store `value` under `key`, overwriting any prior value.
    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), CacheError>;

    /// Fetch the value stored under `key`, if any.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CacheError>;

    /// Remove `key` if present.
    fn delete(&self, key: &[u8]) -> Result<(), CacheError>;

    /// Whether `key` is present.
    fn contains(&self, key: &[u8]) -> Result<bool, CacheError> {
        Ok(self.get(key)?.is_some())
    }
}

/// A RocksDB-backed [`BlobCache`].
#[derive(Debug)]
pub struct RocksBlobCache {
    db: rocksdb::DB,
}

impl RocksBlobCache {
    /// Open (creating if absent) a cache rooted at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, path).map_err(|e| CacheError::Backend(e.to_string()))?;
        Ok(Self { db })
    }
}

impl BlobCache for RocksBlobCache {
    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), CacheError> {
        self.db
            .put(key, value)
            .map_err(|e| CacheError::Backend(e.to_string()))
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CacheError> {
        self.db
            .get(key)
            .map_err(|e| CacheError::Backend(e.to_string()))
    }

    fn delete(&self, key: &[u8]) -> Result<(), CacheError> {
        self.db
            .delete(key)
            .map_err(|e| CacheError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_values() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RocksBlobCache::open(dir.path().join("db")).unwrap();

        assert!(cache.get(b"missing").unwrap().is_none());
        cache.put(b"k", b"hello").unwrap();
        assert_eq!(cache.get(b"k").unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(cache.contains(b"k").unwrap());

        cache.delete(b"k").unwrap();
        assert!(!cache.contains(b"k").unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let cache = RocksBlobCache::open(&path).unwrap();
            cache.put(b"persisted", b"1").unwrap();
        }
        let reopened = RocksBlobCache::open(&path).unwrap();
        assert_eq!(
            reopened.get(b"persisted").unwrap().as_deref(),
            Some(&b"1"[..])
        );
    }
}
