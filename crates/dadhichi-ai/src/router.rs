//! Model registry and capability-aware routing.
//!
//! The [`ModelRouter`] owns every registered [`LanguageModel`] and decides which
//! one fulfils a request. This is the seam for cost optimisation, fallback, and
//! load balancing: it routes by explicit id or required capability, retries
//! down a configurable fallback chain on failure, and can price any completion
//! through an attached [`CostTable`].

use crate::cost::CostTable;
use crate::provider::{LanguageModel, ModelCapabilities, ProviderError, ProviderResult};
use crate::types::{Completion, CompletionRequest};
use std::collections::HashMap;
use std::sync::Arc;

/// A registry of providers with a routing policy over them.
#[derive(Clone, Default)]
pub struct ModelRouter {
    providers: HashMap<String, Arc<dyn LanguageModel>>,
    default_id: Option<String>,
    /// Ordered ids tried, in turn, when the primary choice errors.
    fallback_chain: Vec<String>,
    costs: CostTable,
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRouter")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default", &self.default_id)
            .field("fallback_chain", &self.fallback_chain)
            .finish()
    }
}

impl ModelRouter {
    /// Create an empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider`. The first provider registered becomes the default.
    pub fn register(&mut self, provider: Arc<dyn LanguageModel>) -> &mut Self {
        let id = provider.id().to_string();
        self.default_id.get_or_insert_with(|| id.clone());
        self.providers.insert(id, provider);
        self
    }

    /// Explicitly set the default provider by id.
    pub fn set_default(&mut self, id: impl Into<String>) -> &mut Self {
        self.default_id = Some(id.into());
        self
    }

    /// Set the ordered fallback chain tried when a call fails.
    pub fn set_fallback_chain(&mut self, ids: impl IntoIterator<Item = String>) -> &mut Self {
        self.fallback_chain = ids.into_iter().collect();
        self
    }

    /// Attach a cost table so completions can be priced.
    pub fn set_cost_table(&mut self, costs: CostTable) -> &mut Self {
        self.costs = costs;
        self
    }

    /// The attached cost table.
    pub fn costs(&self) -> &CostTable {
        &self.costs
    }

    /// Look up a provider by id.
    pub fn provider(&self, id: &str) -> Option<Arc<dyn LanguageModel>> {
        self.providers.get(id).cloned()
    }

    /// The first provider satisfying a capability predicate, if any.
    pub fn find_capable(
        &self,
        pred: impl Fn(ModelCapabilities) -> bool,
    ) -> Option<Arc<dyn LanguageModel>> {
        self.providers
            .values()
            .find(|p| pred(p.capabilities()))
            .cloned()
    }

    /// Resolve which provider should serve `request`.
    ///
    /// Routing precedence:
    /// 1. an exact provider id match on `request.model`;
    /// 2. otherwise the configured default provider.
    pub fn route(&self, request: &CompletionRequest) -> ProviderResult<Arc<dyn LanguageModel>> {
        if let Some(p) = self.providers.get(&request.model) {
            return Ok(p.clone());
        }
        self.default_id
            .as_ref()
            .and_then(|id| self.providers.get(id))
            .cloned()
            .ok_or_else(|| ProviderError::UnknownModel(request.model.clone()))
    }

    /// Route and complete in one call (no fallback).
    pub async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        let provider = self.route(&request)?;
        provider.complete(request).await
    }

    /// Complete with fallback: try the routed provider first, then each id in
    /// the fallback chain, returning the first success.
    ///
    /// This is how the IDE stays responsive when a hosted provider is rate
    /// limited or down — it transparently drops to, say, a local model. The last
    /// error is returned if every attempt fails.
    pub async fn complete_resilient(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<Completion> {
        let mut attempts: Vec<Arc<dyn LanguageModel>> = Vec::new();
        if let Ok(primary) = self.route(&request) {
            attempts.push(primary);
        }
        for id in &self.fallback_chain {
            if let Some(p) = self.providers.get(id) {
                attempts.push(p.clone());
            }
        }
        if attempts.is_empty() {
            return Err(ProviderError::UnknownModel(request.model.clone()));
        }

        let mut last_err = None;
        for provider in attempts {
            match provider.complete(request.clone()).await {
                Ok(completion) => return Ok(completion),
                Err(err) => {
                    tracing::warn!(provider = provider.id(), %err, "provider failed, falling back");
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ProviderError::UnknownModel(request.model.clone())))
    }

    /// Cost in USD of a completed exchange, priced by the attached cost table.
    pub fn cost_of(&self, completion: &Completion) -> f64 {
        self.costs.cost(&completion.model, completion.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::ModelPricing;
    use crate::provider::MockProvider;
    use crate::types::{Message, Usage};
    use async_trait::async_trait;

    /// A provider that always fails, to exercise the fallback path.
    #[derive(Debug)]
    struct BrokenProvider;

    #[async_trait]
    impl LanguageModel for BrokenProvider {
        fn id(&self) -> &str {
            "broken"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: CompletionRequest) -> ProviderResult<Completion> {
            Err(ProviderError::Transport("simulated outage".into()))
        }
    }

    #[tokio::test]
    async fn resilient_completion_falls_back_to_working_provider() {
        let mut router = ModelRouter::new();
        router.register(Arc::new(BrokenProvider));
        router.register(Arc::new(MockProvider::default()));
        router.set_default("broken");
        router.set_fallback_chain(["mock".to_string()]);

        let req = CompletionRequest::new("broken").message(Message::user("hello"));
        let out = router.complete_resilient(req).await.unwrap();
        assert!(out.content.contains("hello"));
    }

    #[tokio::test]
    async fn resilient_completion_errors_when_all_fail() {
        let mut router = ModelRouter::new();
        router.register(Arc::new(BrokenProvider));
        let req = CompletionRequest::new("broken").message(Message::user("hi"));
        assert!(router.complete_resilient(req).await.is_err());
    }

    #[test]
    fn router_prices_completion() {
        let mut router = ModelRouter::new();
        router.set_cost_table(CostTable::new().with("gpt-x", ModelPricing::new(3.0, 15.0)));
        let completion = Completion {
            content: "x".into(),
            model: "gpt-x".into(),
            usage: Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 0,
            },
        };
        assert!((router.cost_of(&completion) - 3.0).abs() < 1e-9);
    }
}
