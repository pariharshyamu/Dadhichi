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
    /// For a `Role::Assistant` turn that requested tools natively, the calls it
    /// made — so the turn can be re-sent faithfully on the next request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Marks this turn as a prompt-cache breakpoint. Providers that support
    /// server-side prompt caching (Anthropic) cache the prefix up to and
    /// including this message; the rest ignore it. Set it on large, stable
    /// context (system prompts, pinned files) that recurs across requests.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache: bool,
}

impl Message {
    /// A system/steering message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            cache: false,
        }
    }
    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            cache: false,
        }
    }
    /// An assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            cache: false,
        }
    }

    /// An assistant turn that requested native tool calls (its optional text
    /// plus the calls), so it round-trips into the next request faithfully.
    pub fn assistant_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls,
            cache: false,
        }
    }

    /// A tool-result message answering the call with `id`.
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(id.into()),
            tool_calls: Vec::new(),
            cache: false,
        }
    }

    /// Mark this message as a prompt-cache breakpoint, returning `self`.
    pub fn cached(mut self) -> Self {
        self.cache = true;
        self
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

/// A tool the model may call, in provider-neutral form. Mirrors the JSON-Schema
/// function-definition shape OpenAI, Anthropic, and Ollama all accept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// The tool's unique name, e.g. `"fs.read"`.
    pub name: String,
    /// A one-line description the model uses to decide when to call it.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

/// A tool invocation the model asked for, parsed from a native tool-calling
/// response. `id` correlates the later [`Role::Tool`] result back to this call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id (echoed back in the tool result message).
    pub id: String,
    /// The tool name to invoke.
    pub name: String,
    /// The arguments object (already parsed from the provider's JSON string).
    pub arguments: serde_json::Value,
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
    /// Tools the model may call natively. Empty ⇒ no tools advertised (the
    /// caller falls back to a text protocol). Providers without tool support
    /// ignore this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
}

impl CompletionRequest {
    /// Start a request for `model` with an empty conversation.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            params: GenerationParams::default(),
            tools: Vec::new(),
        }
    }

    /// Append a message, returning `self` for chaining.
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Advertise tools the model may call natively, returning `self`.
    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
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
    /// Of the prompt tokens, how many were served from the provider's prompt
    /// cache (billed at a large discount). `0` when caching didn't apply or the
    /// provider doesn't report it. Purely informational — `prompt_tokens`
    /// already includes these.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_prompt_tokens: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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
    /// Native tool calls the model requested, if any. Empty when the model
    /// answered in text (or the provider does not support tool calling).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
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
