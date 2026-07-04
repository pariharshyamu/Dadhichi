//! An Anthropic Messages API provider.
//!
//! Anthropic differs from the OpenAI schema in three ways this module handles:
//! the system prompt is a top-level `system` field (not a message), `max_tokens`
//! is mandatory, and the stream is a sequence of typed events
//! (`content_block_delta`, `message_delta`, …) rather than choice deltas.

use super::sse::SseDecoder;
use crate::provider::{LanguageModel, ModelCapabilities, ProviderError, ProviderResult};
use crate::types::{Completion, CompletionRequest, Role, StreamChunk, Usage};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;

/// Anthropic's `anthropic-version` header value.
const API_VERSION: &str = "2023-06-01";
/// Anthropic requires `max_tokens`; used when the request leaves it unset.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Connection settings for the Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// A provider for Anthropic's hosted API.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Override the base URL (e.g. a proxy or gateway).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/messages", self.base_url)
    }

    fn request(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(self.endpoint())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
    }
}

/// Build the JSON request body, splitting out system messages and defaulting
/// `max_tokens`. Pure, so the wire format is unit-testable.
fn build_body(request: &CompletionRequest, stream: bool) -> serde_json::Value {
    let mut system = String::new();
    let mut messages = Vec::new();
    for m in &request.messages {
        match m.role {
            Role::System => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.content);
            }
            role => messages.push(serde_json::json!({
                "role": if matches!(role, Role::Assistant) { "assistant" } else { "user" },
                "content": m.content,
            })),
        }
    }

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "temperature": request.params.temperature,
        "top_p": request.params.top_p,
        "stream": stream,
    });
    if !system.is_empty() {
        body["system"] = serde_json::json!(system);
    }
    if !request.params.stop.is_empty() {
        body["stop_sequences"] = serde_json::json!(request.params.stop);
    }
    body
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    model: String,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default, rename = "type")]
    _kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Parse a non-streamed Messages response.
fn parse_completion(json: &str, fallback_model: &str) -> ProviderResult<Completion> {
    let resp: ApiResponse = serde_json::from_str(json)
        .map_err(|e| ProviderError::Rejected(format!("malformed response: {e}")))?;
    let content: String = resp.content.iter().map(|b| b.text.as_str()).collect();
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
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
        },
    })
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Parse one streamed SSE event into a [`StreamChunk`], if relevant.
fn parse_stream_chunk(json: &str) -> Option<StreamChunk> {
    let event: StreamEvent = serde_json::from_str(json).ok()?;
    match event.kind.as_str() {
        "content_block_delta" => {
            let text = event.delta?.text?;
            Some(StreamChunk {
                delta: text,
                finish_reason: None,
            })
        }
        "message_delta" => {
            let reason = event.delta.and_then(|d| d.stop_reason)?;
            Some(StreamChunk {
                delta: String::new(),
                finish_reason: Some(reason),
            })
        }
        "message_stop" => Some(StreamChunk {
            delta: String::new(),
            finish_reason: Some("stop".into()),
        }),
        _ => None,
    }
}

#[async_trait]
impl LanguageModel for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            vision: true,
            reasoning: true,
            ..Default::default()
        }
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        let body = build_body(&request, false);
        let resp = self
            .request(&body)
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
            .request(&body)
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
    fn system_message_is_hoisted_out() {
        let req = CompletionRequest::new("claude-sonnet-5")
            .message(Message::system("be terse"))
            .message(Message::user("hi"));
        let body = build_body(&req, false);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn parses_completion_fixture() {
        let json = r#"{
            "model": "claude-sonnet-5",
            "content": [{"type":"text","text":"Hi "},{"type":"text","text":"there"}],
            "usage": {"input_tokens": 8, "output_tokens": 2}
        }"#;
        let c = parse_completion(json, "fallback").unwrap();
        assert_eq!(c.content, "Hi there");
        assert_eq!(c.usage.prompt_tokens, 8);
        assert_eq!(c.usage.completion_tokens, 2);
    }

    #[test]
    fn parses_stream_events() {
        let text = parse_stream_chunk(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hey"}}"#,
        )
        .unwrap();
        assert_eq!(text.delta, "Hey");

        let stop =
            parse_stream_chunk(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#)
                .unwrap();
        assert_eq!(stop.finish_reason.as_deref(), Some("end_turn"));

        assert!(parse_stream_chunk(r#"{"type":"ping"}"#).is_none());
    }
}
