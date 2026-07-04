//! # dadhichi-vector
//!
//! Vector storage and nearest-neighbour search — the substrate for semantic
//! code search and long-term agent memory. Text is turned into an embedding by
//! an [`dadhichi_ai::EmbeddingModel`](https://docs.rs) and stored here with a
//! payload; recall is by cosine similarity to a query embedding.
//!
//! The [`VectorStore`] trait is the query surface. [`InMemoryVectorStore`] is a
//! correct brute-force backend (exact kNN) used for tests and small workspaces;
//! the production backend is LanceDB, which implements the same trait with an
//! on-disk ANN index. Keeping search behind the trait means swapping backends
//! never touches callers.
//!
//! ```
//! use dadhichi_vector::{InMemoryVectorStore, VectorStore, Record};
//!
//! let mut store = InMemoryVectorStore::new(3);
//! store.upsert(Record::new("a", vec![1.0, 0.0, 0.0], serde_json::json!({"t": "x"}))).unwrap();
//! store.upsert(Record::new("b", vec![0.0, 1.0, 0.0], serde_json::json!({"t": "y"}))).unwrap();
//!
//! let hits = store.search(&[0.9, 0.1, 0.0], 1).unwrap();
//! assert_eq!(hits[0].id, "a");
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors from the vector store.
#[derive(Debug, Error)]
pub enum VectorError {
    /// A vector's dimensionality did not match the store's.
    #[error("dimension mismatch: expected {expected}, got {got}")]
    Dimension {
        /// The store's configured dimensionality.
        expected: usize,
        /// The offending vector's length.
        got: usize,
    },
}

/// A stored item: an id, its embedding, and an arbitrary JSON payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Caller-chosen unique id (upserting the same id replaces the record).
    pub id: String,
    /// The embedding vector.
    pub vector: Vec<f32>,
    /// Application payload (e.g. the source snippet and its location).
    pub payload: serde_json::Value,
}

impl Record {
    /// Build a record.
    pub fn new(id: impl Into<String>, vector: Vec<f32>, payload: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            vector,
            payload,
        }
    }
}

/// A single search result: the matched record plus its similarity score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// The matched record's id.
    pub id: String,
    /// Cosine similarity to the query in `-1.0..=1.0` (higher is closer).
    pub score: f32,
    /// The matched record's payload.
    pub payload: serde_json::Value,
}

/// A store of embeddings supporting nearest-neighbour search.
pub trait VectorStore {
    /// Insert or replace a record.
    fn upsert(&mut self, record: Record) -> Result<(), VectorError>;

    /// Remove a record by id. Returns whether it existed.
    fn remove(&mut self, id: &str) -> bool;

    /// The `k` records most similar to `query`, best first.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>;

    /// Number of stored records.
    fn len(&self) -> usize;

    /// Whether the store is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Cosine similarity of two equal-length vectors. Returns `0.0` if either has
/// zero magnitude (undefined direction).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// An exact, brute-force [`VectorStore`] held in memory.
#[derive(Debug, Default)]
pub struct InMemoryVectorStore {
    dims: usize,
    records: Vec<Record>,
}

impl InMemoryVectorStore {
    /// Create a store for vectors of dimensionality `dims`.
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            records: Vec::new(),
        }
    }

    /// The dimensionality this store accepts.
    pub fn dimensions(&self) -> usize {
        self.dims
    }

    fn check_dims(&self, v: &[f32]) -> Result<(), VectorError> {
        if v.len() == self.dims {
            Ok(())
        } else {
            Err(VectorError::Dimension {
                expected: self.dims,
                got: v.len(),
            })
        }
    }
}

impl VectorStore for InMemoryVectorStore {
    fn upsert(&mut self, record: Record) -> Result<(), VectorError> {
        self.check_dims(&record.vector)?;
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == record.id) {
            *existing = record;
        } else {
            self.records.push(record);
        }
        Ok(())
    }

    fn remove(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.id != id);
        self.records.len() != before
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        self.check_dims(query)?;
        let mut scored: Vec<Hit> = self
            .records
            .iter()
            .map(|r| Hit {
                id: r.id.clone(),
                score: cosine_similarity(query, &r.vector),
                payload: r.payload.clone(),
            })
            .collect();
        // Descending by score; total_cmp gives a well-defined order for floats.
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        scored.truncate(k);
        Ok(scored)
    }

    fn len(&self) -> usize {
        self.records.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> InMemoryVectorStore {
        let mut s = InMemoryVectorStore::new(3);
        s.upsert(Record::new("x", vec![1.0, 0.0, 0.0], serde_json::json!({})))
            .unwrap();
        s.upsert(Record::new("y", vec![0.0, 1.0, 0.0], serde_json::json!({})))
            .unwrap();
        s.upsert(Record::new("z", vec![0.0, 0.0, 1.0], serde_json::json!({})))
            .unwrap();
        s
    }

    #[test]
    fn cosine_of_orthogonal_is_zero_and_parallel_is_one() {
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 1.0], &[2.0, 2.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn search_ranks_by_similarity() {
        let s = store();
        let hits = s.search(&[0.9, 0.1, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "x");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn upsert_replaces_and_remove_deletes() {
        let mut s = store();
        s.upsert(Record::new(
            "x",
            vec![0.0, 0.0, 1.0],
            serde_json::json!({ "v": 2 }),
        ))
        .unwrap();
        assert_eq!(s.len(), 3, "upsert replaces, not appends");
        let top = s.search(&[0.0, 0.0, 1.0], 1).unwrap();
        // Both x (now z-aligned) and z match; either is acceptable, score ~1.
        assert!(top[0].score > 0.99);

        assert!(s.remove("x"));
        assert!(!s.remove("x"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let mut s = InMemoryVectorStore::new(3);
        assert!(
            s.upsert(Record::new("bad", vec![1.0, 2.0], serde_json::json!({})))
                .is_err()
        );
        assert!(s.search(&[1.0], 1).is_err());
    }
}
