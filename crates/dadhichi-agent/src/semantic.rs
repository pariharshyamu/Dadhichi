//! Semantic long-term memory: embed-and-recall over a vector store.
//!
//! Where [`Memory`](crate::Memory) recalls by keyword, [`SemanticMemory`] recalls
//! by *meaning*: each remembered item is embedded (via any
//! [`EmbeddingModel`](dadhichi_ai::EmbeddingModel)) and stored in a
//! [`VectorStore`], so a query retrieves the nearest items by cosine similarity
//! even when the wording differs. This is the substrate for an agent recalling
//! relevant past context and for semantic code search.
//!
//! The vector store here is the in-memory reference; swapping in the LanceDB
//! backend (same [`VectorStore`] trait) makes recall durable and scalable
//! without changing this type.

use dadhichi_ai::{EmbeddingModel, ProviderResult};
use dadhichi_vector::{Hit, InMemoryVectorStore, Record, VectorStore};
use std::sync::Arc;

/// An embedding-backed memory supporting semantic recall.
pub struct SemanticMemory {
    embedder: Arc<dyn EmbeddingModel>,
    store: InMemoryVectorStore,
    next_id: u64,
}

impl std::fmt::Debug for SemanticMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticMemory")
            .field("dimensions", &self.embedder.dimensions())
            .field("items", &self.store.len())
            .finish()
    }
}

impl SemanticMemory {
    /// Create a semantic memory backed by `embedder`.
    pub fn new(embedder: Arc<dyn EmbeddingModel>) -> Self {
        let dims = embedder.dimensions();
        Self {
            embedder,
            store: InMemoryVectorStore::new(dims),
            next_id: 0,
        }
    }

    /// Embed `text` and store it with an optional structured `payload`.
    ///
    /// The stored payload always includes the original `text` under a `text`
    /// key so recall can surface it.
    pub async fn remember(&mut self, text: &str, payload: serde_json::Value) -> ProviderResult<()> {
        let vector = self.embedder.embed_one(text).await?;
        let id = format!("mem-{}", self.next_id);
        self.next_id += 1;
        let record = Record::new(
            id,
            vector,
            serde_json::json!({ "text": text, "data": payload }),
        );
        // A dimension mismatch here is a programming error (embedder vs store),
        // not a runtime condition, so unwrap is acceptable.
        self.store
            .upsert(record)
            .expect("embedder dimensions match store");
        Ok(())
    }

    /// Recall the `k` remembered items most similar in meaning to `query`.
    pub async fn recall(&self, query: &str, k: usize) -> ProviderResult<Vec<Hit>> {
        let vector = self.embedder.embed_one(query).await?;
        Ok(self
            .store
            .search(&vector, k)
            .expect("query dimensions match store"))
    }

    /// Number of remembered items.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether nothing has been remembered yet.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_ai::MockEmbedder;

    #[tokio::test]
    async fn recalls_the_most_similar_item() {
        let mut memory = SemanticMemory::new(Arc::new(MockEmbedder::new(48)));
        memory
            .remember(
                "the borrow checker enforces ownership",
                serde_json::json!({ "id": 1 }),
            )
            .await
            .unwrap();
        memory
            .remember("tokio drives async tasks", serde_json::json!({ "id": 2 }))
            .await
            .unwrap();
        assert_eq!(memory.len(), 2);

        // Querying with the exact remembered text returns it as the top hit
        // (identical embedding → cosine similarity 1.0).
        let hits = memory
            .recall("the borrow checker enforces ownership", 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].payload["text"],
            "the borrow checker enforces ownership"
        );
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn empty_memory_recalls_nothing() {
        let memory = SemanticMemory::new(Arc::new(MockEmbedder::default()));
        assert!(memory.is_empty());
        assert!(memory.recall("anything", 5).await.unwrap().is_empty());
    }
}
