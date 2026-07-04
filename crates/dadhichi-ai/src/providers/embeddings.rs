//! An OpenAI-compatible embeddings provider (`/v1/embeddings`).

use crate::embedding::EmbeddingModel;
use crate::provider::{ProviderError, ProviderResult};
use async_trait::async_trait;
use serde::Deserialize;

/// Connection settings for an OpenAI-compatible embeddings endpoint. Works for
/// OpenAI and any compatible server (Ollama, LM Studio, vLLM).
#[derive(Debug, Clone)]
pub struct OpenAiEmbedder {
    base_url: String,
    api_key: Option<String>,
    model: String,
    dims: usize,
    client: reqwest::Client,
}

impl OpenAiEmbedder {
    /// A custom endpoint. `dims` is the model's output dimensionality.
    pub fn custom(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        dims: usize,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
            dims,
            client: reqwest::Client::new(),
        }
    }

    /// OpenAI's `text-embedding-3-small` (1536 dimensions).
    pub fn openai_small(api_key: impl Into<String>) -> Self {
        Self::custom(
            "https://api.openai.com/v1",
            Some(api_key.into()),
            "text-embedding-3-small",
            1536,
        )
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

/// Parse an embeddings response, restoring input order by `index`.
fn parse_embeddings(json: &str) -> ProviderResult<Vec<Vec<f32>>> {
    let resp: EmbeddingResponse = serde_json::from_str(json)
        .map_err(|e| ProviderError::Rejected(format!("malformed embeddings response: {e}")))?;
    let mut data = resp.data;
    data.sort_by_key(|d| d.index);
    Ok(data.into_iter().map(|d| d.embedding).collect())
}

#[async_trait]
impl EmbeddingModel for OpenAiEmbedder {
    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[String]) -> ProviderResult<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let mut req = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
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
        parse_embeddings(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reorders_embeddings() {
        let json = r#"{
            "data": [
                { "embedding": [0.2, 0.3], "index": 1 },
                { "embedding": [0.0, 0.1], "index": 0 }
            ],
            "model": "text-embedding-3-small"
        }"#;
        let out = parse_embeddings(json).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.0, 0.1]); // index 0 first
        assert_eq!(out[1], vec![0.2, 0.3]);
    }
}
