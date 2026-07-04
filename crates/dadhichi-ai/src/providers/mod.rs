//! Concrete HTTP-backed [`LanguageModel`](crate::provider::LanguageModel)
//! implementations, gated behind the `http` feature.
//!
//! Two providers cover the entire supported matrix:
//!
//! - [`OpenAiProvider`] speaks the OpenAI `/chat/completions` schema, which
//!   OpenRouter, Together, vLLM, LM Studio, and Ollama also implement.
//! - [`AnthropicProvider`] speaks the Anthropic Messages API.
//!
//! Both share the [`sse`] decoder for streaming and expose their request-body
//! and response-parsing logic as pure functions, so the wire formats are
//! unit-tested without any network access.

pub mod anthropic;
pub mod embeddings;
pub mod openai;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use embeddings::OpenAiEmbedder;
pub use openai::OpenAiProvider;
pub use sse::SseDecoder;
