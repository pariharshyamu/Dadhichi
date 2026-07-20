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

/// A progress notice emitted while [`ModelRouter::complete_resilient_with`]
/// retries or falls back, so a frontend can surface "rate limited — retrying".
#[derive(Debug, Clone)]
pub struct RetryNotice {
    /// The provider that just failed.
    pub provider: String,
    /// Which attempt on that provider this was (1-based).
    pub attempt: u32,
    /// How long the loop will wait before the next attempt (0 when falling back
    /// to a different provider rather than retrying the same one).
    pub delay_ms: u64,
    /// The error that triggered the retry/fallback.
    pub reason: String,
    /// True when moving to a different provider rather than retrying this one.
    pub falling_back: bool,
}

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
        self.complete_resilient_with(request, |_| {}).await
    }

    /// Like [`complete_resilient`](Self::complete_resilient) but retries a
    /// transient failure (429 / 5xx / dropped connection) on the same provider
    /// with exponential backoff before moving down the fallback chain, and
    /// reports each retry/fallback through `on_retry` so a frontend can show
    /// "rate limited — retrying in 2s". Fatal errors (auth, bad request,
    /// unknown model) are not retried.
    pub async fn complete_resilient_with(
        &self,
        request: CompletionRequest,
        on_retry: impl Fn(RetryNotice),
    ) -> ProviderResult<Completion> {
        const MAX_ATTEMPTS_PER_PROVIDER: u32 = 3;
        const BASE_DELAY_MS: u64 = 500;

        let mut providers: Vec<Arc<dyn LanguageModel>> = Vec::new();
        if let Ok(primary) = self.route(&request) {
            providers.push(primary);
        }
        for id in &self.fallback_chain {
            if let Some(p) = self.providers.get(id) {
                providers.push(p.clone());
            }
        }
        if providers.is_empty() {
            return Err(ProviderError::UnknownModel(request.model.clone()));
        }

        let last_index = providers.len() - 1;
        let mut last_err = None;
        for (i, provider) in providers.into_iter().enumerate() {
            for attempt in 0..MAX_ATTEMPTS_PER_PROVIDER {
                match provider.complete(request.clone()).await {
                    Ok(completion) => return Ok(completion),
                    Err(err) => {
                        let retryable = err.is_retryable();
                        tracing::warn!(provider = provider.id(), %err, retryable, "provider call failed");
                        // Retry the same provider on a transient error, unless
                        // this was the last attempt for it.
                        if retryable && attempt + 1 < MAX_ATTEMPTS_PER_PROVIDER {
                            let delay = BASE_DELAY_MS * 2u64.pow(attempt);
                            on_retry(RetryNotice {
                                provider: provider.id().to_string(),
                                attempt: attempt + 1,
                                delay_ms: delay,
                                reason: err.to_string(),
                                falling_back: false,
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                            last_err = Some(err);
                            continue;
                        }
                        // Give up on this provider. Fall to the next if there is
                        // one and the error was transient (a fatal error would
                        // fail the same way everywhere — but a different provider
                        // may not have the same auth/model problem, so we still
                        // try the chain).
                        last_err = Some(err);
                        if i < last_index {
                            on_retry(RetryNotice {
                                provider: provider.id().to_string(),
                                attempt: attempt + 1,
                                delay_ms: 0,
                                reason: last_err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                                falling_back: true,
                            });
                        }
                        break;
                    }
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
    fn errors_are_classified_as_retryable_or_fatal() {
        assert!(ProviderError::Transport("reset".into()).is_retryable());
        assert!(ProviderError::Rejected("HTTP 429 Too Many Requests".into()).is_retryable());
        assert!(ProviderError::Rejected("model overloaded".into()).is_retryable());
        assert!(ProviderError::Rejected("HTTP 503".into()).is_retryable());
        // Fatal: auth, bad request, unknown model.
        assert!(!ProviderError::Rejected("HTTP 401 invalid api key".into()).is_retryable());
        assert!(!ProviderError::Rejected("HTTP 400 bad request".into()).is_retryable());
        assert!(!ProviderError::UnknownModel("nope".into()).is_retryable());
    }

    /// A provider that fails `fail_times` with a 429, then succeeds.
    #[derive(Debug)]
    struct FlakyProvider {
        remaining: std::sync::Mutex<u32>,
    }

    #[async_trait]
    impl LanguageModel for FlakyProvider {
        fn id(&self) -> &str {
            "flaky"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
            let fail = {
                let mut r = self.remaining.lock().unwrap();
                if *r > 0 { *r -= 1; true } else { false }
            };
            if fail {
                Err(ProviderError::Rejected("HTTP 429 too many requests".into()))
            } else {
                Ok(Completion {
                    content: "recovered".into(),
                    model: request.model,
                    usage: Usage::default(),
                    tool_calls: Vec::new(),
                })
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_are_retried_then_succeed() {
        // Fails twice (429), succeeds on the third attempt — all on the same
        // provider, so no fallback is needed. start_paused makes the backoff
        // sleeps virtual (instant).
        let mut router = ModelRouter::new();
        router.register(Arc::new(FlakyProvider { remaining: std::sync::Mutex::new(2) }));

        let retries = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let r2 = retries.clone();
        let req = CompletionRequest::new("flaky").message(Message::user("hi"));
        let out = router
            .complete_resilient_with(req, move |_n| *r2.lock().unwrap() += 1)
            .await
            .unwrap();
        assert_eq!(out.content, "recovered");
        assert_eq!(*retries.lock().unwrap(), 2, "two retries before success");
    }

    #[tokio::test]
    async fn a_fatal_error_is_not_retried() {
        // A 401 must fail immediately without burning retries.
        #[derive(Debug)]
        struct AuthFail(std::sync::Mutex<u32>);
        #[async_trait]
        impl LanguageModel for AuthFail {
            fn id(&self) -> &str { "auth" }
            fn capabilities(&self) -> ModelCapabilities { ModelCapabilities::default() }
            async fn complete(&self, _r: CompletionRequest) -> ProviderResult<Completion> {
                *self.0.lock().unwrap() += 1;
                Err(ProviderError::Rejected("HTTP 401 invalid key".into()))
            }
        }
        let provider = Arc::new(AuthFail(std::sync::Mutex::new(0)));
        let mut router = ModelRouter::new();
        router.register(provider.clone());
        let req = CompletionRequest::new("auth").message(Message::user("hi"));
        assert!(router.complete_resilient(req).await.is_err());
        assert_eq!(*provider.0.lock().unwrap(), 1, "fatal error called exactly once");
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
                cached_prompt_tokens: 0,
            },
            tool_calls: Vec::new(),
        };
        assert!((router.cost_of(&completion) - 3.0).abs() < 1e-9);
    }
}
