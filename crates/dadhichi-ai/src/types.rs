//! Provider-neutral request and response types.
//!
//! These deliberately mirror the lowest common denominator across OpenAI,
//! Anthropic, Gemini, and local runtimes (llama.cpp, Ollama, vLLM) so a single
//! [`LanguageModel`](crate::provider::LanguageModel) trait can front all of them.

use serde::{Deserialize, Serialize};

/// Who authored a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Steering instructions that frame the whole conversation.
    System,
    /// A human (or an upstream agent acting on their behalf).
    User,
    /// The model's own turns.
    Assistant,
    /// The result of a tool the model asked to invoke.
    Tool,
}

/// One turn in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The author of this turn.
    pub role: Role,
    /// The textual content of the turn.
    pub content: String,
    /// For `Role::Tool` messages, the id of the tool call being answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A system/steering message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
        }
    }
    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
        }
    }
    /// An assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
        }
    }
}

/// Sampling and budgeting knobs common to essentially every provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Softmax temperature. `0.0` is greedy/deterministic.
    pub temperature: f32,
    /// Nucleus sampling cutoff.
    pub top_p: f32,
    /// Hard cap on generated tokens, if any.
    pub max_tokens: Option<u32>,
    /// Stop sequences that end generation early.
    pub stop: Vec<String>,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 1.0,
            max_tokens: None,
            stop: Vec::new(),
        }
    }
}

/// A full completion request: the model to target, the conversation, and knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Model identifier as understood by the target provider.
    pub model: String,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Sampling parameters.
    #[serde(default)]
    pub params: GenerationParams,
}

impl CompletionRequest {
    /// Start a request for `model` with an empty conversation.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            params: GenerationParams::default(),
        }
    }

    /// Append a message, returning `self` for chaining.
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }
}

/// Token accounting returned alongside a completion, used for cost tracking.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: u32,
    /// Tokens produced in the completion.
    pub completion_tokens: u32,
}

impl Usage {
    /// Total tokens billed for the exchange.
    pub fn total(&self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// A non-streamed completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// The generated text.
    pub content: String,
    /// The model that produced it (post-routing).
    pub model: String,
    /// Token accounting.
    pub usage: Usage,
}

/// An incremental chunk of a streamed completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    /// The text delta since the previous chunk.
    pub delta: String,
    /// Set on the final chunk with a reason such as `"stop"` or `"length"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}
