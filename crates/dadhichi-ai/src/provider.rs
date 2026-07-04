//! The core provider abstraction plus a deterministic mock implementation.

use crate::types::{Completion, CompletionRequest, StreamChunk, Usage};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use thiserror::Error;

/// Errors surfaced by a language-model provider.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The provider or endpoint could not be reached.
    #[error("transport error: {0}")]
    Transport(String),
    /// The provider rejected the request (auth, quota, malformed input, …).
    #[error("provider rejected request: {0}")]
    Rejected(String),
    /// The requested model is not served by this provider.
    #[error("unknown model: {0}")]
    UnknownModel(String),
}

/// Result type for provider calls.
pub type ProviderResult<T> = Result<T, ProviderError>;

/// A capability descriptor lets the router pick an appropriate model for a task.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCapabilities {
    /// Handles image inputs.
    pub vision: bool,
    /// Exposes explicit chain-of-thought / reasoning effort.
    pub reasoning: bool,
    /// Can emit structured tool calls.
    pub tools: bool,
    /// Produces embeddings rather than chat completions.
    pub embeddings: bool,
    /// Runs fully on-device (no network, offline-first).
    pub local: bool,
}

/// A uniform interface over every LLM backend Dadhichi supports.
///
/// Implementors wrap OpenAI, Anthropic, Gemini, Ollama, llama.cpp, and so on.
/// The rest of the IDE only ever sees this trait, which is what makes the IDE
/// *model-agnostic*.
#[async_trait]
pub trait LanguageModel: Send + Sync {
    /// A stable identifier for this provider instance, e.g. `"anthropic"`.
    fn id(&self) -> &str;

    /// What this model can do, so the router can match it to a task.
    fn capabilities(&self) -> ModelCapabilities;

    /// Produce a single, complete response.
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion>;

    /// Produce a response as a stream of deltas.
    ///
    /// The default implementation degrades gracefully by calling
    /// [`complete`](Self::complete) and emitting the result as one chunk, so a
    /// provider without native streaming still satisfies the contract.
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<BoxStream<'static, ProviderResult<StreamChunk>>> {
        let completion = self.complete(request).await?;
        let chunk = StreamChunk {
            delta: completion.content,
            finish_reason: Some("stop".into()),
        };
        Ok(stream::once(async move { Ok(chunk) }).boxed())
    }
}

/// A deterministic, offline provider used for tests, demos, and offline mode.
///
/// It echoes the last user message with a fixed prefix so behaviour is
/// reproducible without any network access.
#[derive(Debug, Clone)]
pub struct MockProvider {
    id: String,
    prefix: String,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self {
            id: "mock".into(),
            prefix: "[dadhichi-mock] ".into(),
        }
    }
}

impl MockProvider {
    /// Create a mock provider with a custom id and echo prefix.
    pub fn new(id: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            prefix: prefix.into(),
        }
    }
}

#[async_trait]
impl LanguageModel for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            local: true,
            tools: true,
            ..Default::default()
        }
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::types::Role::User))
            .map(|m| m.content.as_str())
            .unwrap_or("<no user message>");

        let content = format!("{}{}", self.prefix, last_user);
        let usage = Usage {
            prompt_tokens: request
                .messages
                .iter()
                .map(|m| m.content.len() as u32 / 4)
                .sum(),
            completion_tokens: content.len() as u32 / 4,
        };
        Ok(Completion {
            content,
            model: request.model,
            usage,
        })
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<BoxStream<'static, ProviderResult<StreamChunk>>> {
        // Emit word-by-word so callers can exercise real streaming paths.
        let completion = self.complete(request).await?;
        let words: Vec<String> = completion
            .content
            .split_inclusive(' ')
            .map(|s| s.to_string())
            .collect();
        let n = words.len();
        let s = stream::iter(words.into_iter().enumerate()).map(move |(i, w)| {
            Ok(StreamChunk {
                delta: w,
                finish_reason: (i + 1 == n).then(|| "stop".to_string()),
            })
        });
        Ok(s.boxed())
    }
}
