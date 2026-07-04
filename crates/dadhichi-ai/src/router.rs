//! Model registry and capability-aware routing.
//!
//! The [`ModelRouter`] owns every registered [`LanguageModel`] and decides which
//! one fulfils a request. This is the seam for cost optimisation, fallback, and
//! load balancing: today it routes by explicit id or required capability, and
//! that policy can grow without touching call sites.

use crate::provider::{LanguageModel, ModelCapabilities, ProviderError, ProviderResult};
use crate::types::{Completion, CompletionRequest};
use std::collections::HashMap;
use std::sync::Arc;

/// A registry of providers with a routing policy over them.
#[derive(Clone, Default)]
pub struct ModelRouter {
    providers: HashMap<String, Arc<dyn LanguageModel>>,
    default_id: Option<String>,
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRouter")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default", &self.default_id)
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

    /// Route and complete in one call.
    pub async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        let provider = self.route(&request)?;
        provider.complete(request).await
    }
}
