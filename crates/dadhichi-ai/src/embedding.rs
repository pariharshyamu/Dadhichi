//! Embedding models: text → vector, for semantic search and agent memory.
//!
//! Embeddings are the bridge to [`dadhichi-vector`](https://docs.rs): text is
//! embedded here, stored there, and recalled by cosine similarity. The
//! [`EmbeddingModel`] trait fronts every backend — hosted (OpenAI, Voyage,
//! Cohere) or local (a GGUF model via llama.cpp) — so the memory system is
//! embedder-agnostic. [`MockEmbedder`] is a deterministic, offline embedder for
//! tests.

use crate::provider::ProviderResult;
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// A model that turns text into fixed-length embedding vectors.
#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    /// The dimensionality of the vectors this model produces.
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts, returning one vector per input in order.
    async fn embed(&self, texts: &[String]) -> ProviderResult<Vec<Vec<f32>>>;

    /// Embed a single text.
    async fn embed_one(&self, text: &str) -> ProviderResult<Vec<f32>> {
        let mut out = self.embed(&[text.to_string()]).await?;
        Ok(out.pop().unwrap_or_default())
    }
}

/// A deterministic, offline embedder.
///
/// It hashes `(text, dimension_index)` into each component and L2-normalises the
/// result, so identical text always yields an identical unit vector. It is not
/// semantically meaningful — it exists to exercise the embedding → vector-store
/// → recall pipeline reproducibly without a network or a model file.
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dims: usize,
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self { dims: 64 }
    }
}

impl MockEmbedder {
    /// Create a mock embedder producing `dims`-dimensional vectors.
    pub fn new(dims: usize) -> Self {
        Self { dims: dims.max(1) }
    }

    fn embed_sync(&self, text: &str) -> Vec<f32> {
        let mut v: Vec<f32> = (0..self.dims)
            .map(|i| {
                let mut h = DefaultHasher::new();
                text.hash(&mut h);
                i.hash(&mut h);
                // Map the hash into a centred, bounded component.
                (h.finish() % 2000) as f32 / 1000.0 - 1.0
            })
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[async_trait]
impl EmbeddingModel for MockEmbedder {
    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[String]) -> ProviderResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_sync(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_and_normalised() {
        let embedder = MockEmbedder::new(32);
        let a = embedder.embed_one("hello world").await.unwrap();
        let b = embedder.embed_one("hello world").await.unwrap();
        assert_eq!(a, b, "identical text embeds identically");
        assert_eq!(a.len(), 32);

        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit length");
    }

    #[tokio::test]
    async fn distinct_text_differs() {
        let embedder = MockEmbedder::default();
        let a = embedder.embed_one("apple").await.unwrap();
        let b = embedder.embed_one("orange").await.unwrap();
        assert_ne!(a, b);
    }
}
