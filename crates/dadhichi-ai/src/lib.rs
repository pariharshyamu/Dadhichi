//! # dadhichi-ai
//!
//! A **model-agnostic AI runtime**. Every LLM backend — hosted (OpenAI,
//! Anthropic, Gemini, DeepSeek, Mistral, …) or local (Ollama, llama.cpp, vLLM,
//! Candle) — is exposed through the single [`LanguageModel`] trait. The rest of
//! the IDE talks only to that trait and to the [`ModelRouter`], so swapping or
//! adding a model never touches call sites.
//!
//! ```
//! use dadhichi_ai::{ModelRouter, MockProvider, CompletionRequest, Message};
//! use std::sync::Arc;
//!
//! # async fn demo() {
//! let mut router = ModelRouter::new();
//! router.register(Arc::new(MockProvider::default()));
//!
//! let req = CompletionRequest::new("mock").message(Message::user("hello"));
//! let out = router.complete(req).await.unwrap();
//! assert!(out.content.contains("hello"));
//! # }
//! ```

pub mod cache;
pub mod cost;
pub mod embedding;
pub mod provider;
#[cfg(feature = "http")]
pub mod providers;
pub mod router;
pub mod types;

pub use cache::{CachingModel, CompletionCache};
pub use cost::{CostTable, ModelPricing};
pub use embedding::{EmbeddingModel, MockEmbedder};
pub use provider::{LanguageModel, MockProvider, ModelCapabilities, ProviderError, ProviderResult};
pub use router::ModelRouter;
pub use types::{
    Completion, CompletionRequest, GenerationParams, Message, Role, StreamChunk, Usage,
};

#[cfg(feature = "http")]
pub use providers::{AnthropicProvider, OpenAiEmbedder, OpenAiProvider};

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Arc;

    #[tokio::test]
    async fn router_routes_to_default() {
        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));

        let req = CompletionRequest::new("some-unknown-model").message(Message::user("hi there"));
        let out = router.complete(req).await.unwrap();
        assert!(out.content.ends_with("hi there"));
        assert!(out.usage.total() > 0);
    }

    #[tokio::test]
    async fn streaming_reassembles_to_full_text() {
        let provider = MockProvider::default();
        let req = CompletionRequest::new("mock").message(Message::user("stream me please"));

        let mut stream = provider.stream(req).await.unwrap();
        let mut assembled = String::new();
        while let Some(chunk) = stream.next().await {
            assembled.push_str(&chunk.unwrap().delta);
        }
        assert!(assembled.contains("stream me please"));
    }

    #[tokio::test]
    async fn capability_lookup_finds_local_model() {
        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));
        assert!(router.find_capable(|c| c.local).is_some());
        assert!(router.find_capable(|c| c.vision).is_none());
    }
}
