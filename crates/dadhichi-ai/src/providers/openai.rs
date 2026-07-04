//! An OpenAI-compatible chat-completions provider.
//!
//! The `/v1/chat/completions` schema is a de-facto standard: OpenAI, OpenRouter,
//! Together, vLLM, LM Studio, and Ollama (via its `/v1` shim) all speak it. One
//! implementation therefore fronts all of them — only the base URL and API key
//! differ, which is exactly what [`OpenAiProvider::custom`] captures.

use super::sse::SseDecoder;
use crate::provider::{LanguageModel, ModelCapabilities, ProviderError, ProviderResult};
use crate::types::{Completion, CompletionRequest, Role, StreamChunk, Usage};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;

/// Connection settings for an OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    id: String,
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// A provider against an arbitrary OpenAI-compatible `base_url`.
    ///
    /// `base_url` is the root that precedes `/chat/completions`, e.g.
    /// `https://api.openai.com/v1`.
    pub fn custom(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// OpenAI's hosted API.
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::custom("openai", "https://api.openai.com/v1", Some(api_key.into()))
    }

    /// OpenRouter's aggregating API.
    pub fn openrouter(api_key: impl Into<String>) -> Self {
        Self::custom(
            "openrouter",
            "https://openrouter.ai/api/v1",
            Some(api_key.into()),
        )
    }

    /// A local Ollama server (no API key required).
    pub fn ollama() -> Self {
        Self::custom("ollama", "http://localhost:11434/v1", None)
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }
}

/// Build the JSON request body for a chat completion.
///
/// Pulled out as a pure function so the wire format can be tested without a
/// network round-trip.
fn build_body(request: &CompletionRequest, stream: bool) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "temperature": request.params.temperature,
        "top_p": request.params.top_p,
        "stream": stream,
    });
    if let Some(max) = request.params.max_tokens {
        body["max_tokens"] = serde_json::json!(max);
    }
    if !request.params.stop.is_empty() {
        body["stop"] = serde_json::json!(request.params.stop);
    }
    body
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    model: String,
    choices: Vec<ApiChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiMessage,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// Parse a non-streamed completion response body.
fn parse_completion(json: &str, fallback_model: &str) -> ProviderResult<Completion> {
    let resp: ApiResponse = serde_json::from_str(json)
        .map_err(|e| ProviderError::Rejected(format!("malformed response: {e}")))?;
    let content = resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| ProviderError::Rejected("response had no choices".into()))?;
    let usage = resp.usage.unwrap_or_default();
    let model = if resp.model.is_empty() {
        fallback_model.to_string()
    } else {
        resp.model
    };
    Ok(Completion {
        content,
        model,
        usage: Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
        },
    })
}

#[derive(Deserialize)]
struct StreamResponse {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

/// Parse one streamed SSE payload into a [`StreamChunk`], if it carries text or
/// a finish reason.
fn parse_stream_chunk(json: &str) -> Option<StreamChunk> {
    let resp: StreamResponse = serde_json::from_str(json).ok()?;
    let choice = resp.choices.into_iter().next()?;
    let delta = choice.delta.content.unwrap_or_default();
    if delta.is_empty() && choice.finish_reason.is_none() {
        return None;
    }
    Some(StreamChunk {
        delta,
        finish_reason: choice.finish_reason,
    })
}

#[async_trait]
impl LanguageModel for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            vision: true,
            ..Default::default()
        }
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        let body = build_body(&request, false);
        let resp = self
            .authed(self.client.post(self.endpoint()))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Rejected(format!("HTTP {status}: {text}")));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        parse_completion(&text, &request.model)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<BoxStream<'static, ProviderResult<StreamChunk>>> {
        let body = build_body(&request, true);
        let resp = self
            .authed(self.client.post(self.endpoint()))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Rejected(format!("HTTP {status}: {text}")));
        }

        let mut decoder = SseDecoder::new();
        let byte_stream = resp.bytes_stream();
        let chunk_stream = byte_stream.flat_map(move |item| {
            let chunks = match item {
                Ok(bytes) => decoder
                    .feed(&bytes)
                    .iter()
                    .filter_map(|p| parse_stream_chunk(p))
                    .map(Ok)
                    .collect::<Vec<_>>(),
                Err(e) => vec![Err(ProviderError::Transport(e.to_string()))],
            };
            futures::stream::iter(chunks)
        });
        Ok(chunk_stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn body_includes_model_and_messages() {
        let req = CompletionRequest::new("gpt-4o")
            .message(Message::system("sys"))
            .message(Message::user("hi"));
        let body = build_body(&req, false);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn parses_completion_fixture() {
        let json = r#"{
            "model": "gpt-4o-mini",
            "choices": [{"message": {"role": "assistant", "content": "Hello!"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        }"#;
        let c = parse_completion(json, "fallback").unwrap();
        assert_eq!(c.content, "Hello!");
        assert_eq!(c.model, "gpt-4o-mini");
        assert_eq!(c.usage.total(), 15);
    }

    #[test]
    fn empty_choices_is_an_error() {
        let json = r#"{"model":"m","choices":[]}"#;
        assert!(parse_completion(json, "m").is_err());
    }

    #[test]
    fn parses_stream_chunk_and_finish() {
        let c = parse_stream_chunk(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#).unwrap();
        assert_eq!(c.delta, "Hel");
        assert!(c.finish_reason.is_none());

        let f = parse_stream_chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).unwrap();
        assert_eq!(f.finish_reason.as_deref(), Some("stop"));

        assert!(parse_stream_chunk(r#"{"choices":[{"delta":{}}]}"#).is_none());
    }
}
