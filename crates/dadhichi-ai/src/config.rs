//! Provider bootstrap from the environment.
//!
//! The [`LanguageModel`](crate::LanguageModel) trait and the concrete providers
//! already accept API keys through their constructors — but *something* has to
//! read those keys and register the right provider at startup. That is this
//! module: it resolves a [`ProviderPlan`] from environment variables and builds
//! a ready-to-use [`ModelRouter`] from it.
//!
//! Recognised variables:
//!
//! | Variable | Effect |
//! | --- | --- |
//! | `ANTHROPIC_API_KEY` | register the Anthropic Messages API provider |
//! | `OPENAI_API_KEY` | register an OpenAI provider (`OPENAI_BASE_URL` overrides the endpoint for Azure / vLLM / LM Studio / proxies) |
//! | `OPENROUTER_API_KEY` | register the OpenRouter provider |
//! | `OLLAMA_HOST` | register a local Ollama provider at that host (no key) |
//! | `DADHICHI_PROVIDER` | force the default provider id (`anthropic`, `openai`, `openrouter`, `ollama`, `mock`); also enables Ollama with its default host |
//!
//! When no key is set the plan is empty and the router falls back to the
//! offline [`MockProvider`](crate::MockProvider), preserving offline-first
//! behaviour. API keys are never logged — [`ProviderSpec`]'s `Debug` redacts
//! them.

use std::fmt;

/// The default local Ollama endpoint (OpenAI-compatible `/v1` shim).
const OLLAMA_DEFAULT_HOST: &str = "http://localhost:11434";

/// The provider id used for the always-present offline fallback.
pub const MOCK_ID: &str = "mock";

/// A single provider to register, resolved from configuration.
///
/// `Debug` deliberately redacts secrets so a plan can be logged safely.
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderSpec {
    /// Anthropic Messages API. Registers with id `anthropic`.
    Anthropic { api_key: String },
    /// OpenAI-compatible chat completions. Registers with id `openai`.
    OpenAi {
        api_key: String,
        /// Endpoint root before `/chat/completions`; `None` uses OpenAI's.
        base_url: Option<String>,
    },
    /// OpenRouter aggregator. Registers with id `openrouter`.
    OpenRouter { api_key: String },
    /// Local Ollama server (no key). Registers with id `ollama`.
    Ollama {
        /// Host root (e.g. `http://localhost:11434`); `None` uses the default.
        host: Option<String>,
    },
}

impl ProviderSpec {
    /// The provider id this spec registers under (matches the runtime `id()`).
    pub fn id(&self) -> &'static str {
        match self {
            ProviderSpec::Anthropic { .. } => "anthropic",
            ProviderSpec::OpenAi { .. } => "openai",
            ProviderSpec::OpenRouter { .. } => "openrouter",
            ProviderSpec::Ollama { .. } => "ollama",
        }
    }
}

impl fmt::Debug for ProviderSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the key material.
        match self {
            ProviderSpec::Anthropic { .. } => f.write_str("Anthropic { api_key: <redacted> }"),
            ProviderSpec::OpenAi { base_url, .. } => f
                .debug_struct("OpenAi")
                .field("api_key", &"<redacted>")
                .field("base_url", base_url)
                .finish(),
            ProviderSpec::OpenRouter { .. } => f.write_str("OpenRouter { api_key: <redacted> }"),
            ProviderSpec::Ollama { host } => f.debug_struct("Ollama").field("host", host).finish(),
        }
    }
}

/// A resolved set of providers plus which one should be the default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderPlan {
    /// Providers to register, in detection order.
    pub specs: Vec<ProviderSpec>,
    /// The id chosen as the router default. `None` means fall back to `mock`.
    pub default_id: Option<String>,
}

impl ProviderPlan {
    /// Resolve a plan from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    /// Resolve a plan from an arbitrary variable lookup.
    ///
    /// Injecting the lookup keeps resolution pure and unit-testable without
    /// touching the real process environment.
    pub fn from_env_with<F>(get: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        // Treat blank/whitespace-only values as unset — a common footgun with
        // `export OPENAI_API_KEY=` leaving an empty string behind.
        let read = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let forced = read("DADHICHI_PROVIDER").map(|v| v.to_ascii_lowercase());

        let mut specs = Vec::new();

        if let Some(api_key) = read("ANTHROPIC_API_KEY") {
            specs.push(ProviderSpec::Anthropic { api_key });
        }
        if let Some(api_key) = read("OPENAI_API_KEY") {
            specs.push(ProviderSpec::OpenAi {
                api_key,
                base_url: read("OPENAI_BASE_URL"),
            });
        }
        if let Some(api_key) = read("OPENROUTER_API_KEY") {
            specs.push(ProviderSpec::OpenRouter { api_key });
        }
        // Ollama needs no key: enable it if a host is given, or if it is the
        // explicitly requested provider.
        let ollama_host = read("OLLAMA_HOST");
        if ollama_host.is_some() || forced.as_deref() == Some("ollama") {
            specs.push(ProviderSpec::Ollama { host: ollama_host });
        }

        // Choose the default. An explicit `DADHICHI_PROVIDER` wins when that
        // provider was actually configured; otherwise the first detected one.
        let default_id = match forced {
            Some(id) if id == MOCK_ID => Some(MOCK_ID.to_string()),
            Some(id) if specs.iter().any(|s| s.id() == id) => Some(id),
            _ => specs.first().map(|s| s.id().to_string()),
        };

        Self { specs, default_id }
    }

    /// Whether any real (non-mock) provider was configured.
    pub fn has_remote(&self) -> bool {
        !self.specs.is_empty()
    }

    /// The model id the runtime should drive by default: the resolved default
    /// provider, or `mock` when nothing is configured.
    pub fn default_model(&self) -> String {
        self.default_id
            .clone()
            .unwrap_or_else(|| MOCK_ID.to_string())
    }

    /// A human-readable, secret-free one-line summary for startup logs.
    pub fn summary(&self) -> String {
        if self.specs.is_empty() {
            return "offline (mock provider only)".to_string();
        }
        let ids: Vec<&str> = self.specs.iter().map(|s| s.id()).collect();
        format!(
            "{} (default: {}, + mock fallback)",
            ids.join(", "),
            self.default_model()
        )
    }
}

#[cfg(feature = "http")]
mod build {
    use super::{MOCK_ID, OLLAMA_DEFAULT_HOST, ProviderPlan, ProviderSpec};
    use crate::provider::MockProvider;
    use crate::providers::{AnthropicProvider, OpenAiProvider};
    use crate::router::ModelRouter;
    use std::sync::Arc;

    /// Normalise an Ollama host into an OpenAI-compatible `/v1` base URL.
    fn ollama_base(host: &str) -> String {
        let trimmed = host.trim_end_matches('/');
        if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        }
    }

    impl ProviderSpec {
        /// Construct the live provider this spec describes.
        fn build(&self) -> Arc<dyn crate::provider::LanguageModel> {
            match self {
                ProviderSpec::Anthropic { api_key } => {
                    Arc::new(AnthropicProvider::new(api_key.clone()))
                }
                ProviderSpec::OpenAi { api_key, base_url } => match base_url {
                    Some(url) => {
                        Arc::new(OpenAiProvider::custom("openai", url, Some(api_key.clone())))
                    }
                    None => Arc::new(OpenAiProvider::openai(api_key.clone())),
                },
                ProviderSpec::OpenRouter { api_key } => {
                    Arc::new(OpenAiProvider::openrouter(api_key.clone()))
                }
                ProviderSpec::Ollama { host } => {
                    let base = ollama_base(host.as_deref().unwrap_or(OLLAMA_DEFAULT_HOST));
                    Arc::new(OpenAiProvider::custom("ollama", base, None))
                }
            }
        }
    }

    impl ProviderPlan {
        /// Build a router that registers every configured provider plus the
        /// offline mock, with the resolved default and a fallback chain that
        /// ends at the mock so the IDE stays responsive if a hosted provider is
        /// down.
        pub fn build_router(&self) -> ModelRouter {
            let mut router = ModelRouter::new();

            for spec in &self.specs {
                router.register(spec.build());
            }
            // Always keep the offline provider available as the last resort.
            router.register(Arc::new(MockProvider::default()));

            router.set_default(self.default_model());

            // Fallback order: the remaining remote providers, then mock.
            let default = self.default_model();
            let mut chain: Vec<String> = self
                .specs
                .iter()
                .map(|s| s.id().to_string())
                .filter(|id| id != &default)
                .collect();
            if default != MOCK_ID {
                chain.push(MOCK_ID.to_string());
            }
            router.set_fallback_chain(chain);

            router
        }
    }

    /// Convenience: resolve the plan from the environment and build the router.
    pub fn router_from_env() -> (ModelRouter, ProviderPlan) {
        let plan = ProviderPlan::from_env();
        let router = plan.build_router();
        (router, plan)
    }
}

#[cfg(feature = "http")]
pub use build::router_from_env;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn no_vars_yields_offline_plan() {
        let plan = ProviderPlan::from_env_with(env(&[]));
        assert!(!plan.has_remote());
        assert_eq!(plan.default_id, None);
        assert_eq!(plan.default_model(), "mock");
        assert_eq!(plan.summary(), "offline (mock provider only)");
    }

    #[test]
    fn anthropic_key_registers_and_defaults() {
        let plan = ProviderPlan::from_env_with(env(&[("ANTHROPIC_API_KEY", "sk-ant-xxx")]));
        assert_eq!(
            plan.specs,
            vec![ProviderSpec::Anthropic {
                api_key: "sk-ant-xxx".into()
            }]
        );
        assert_eq!(plan.default_model(), "anthropic");
        assert!(plan.has_remote());
    }

    #[test]
    fn openai_base_url_override_is_captured() {
        let plan = ProviderPlan::from_env_with(env(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("OPENAI_BASE_URL", "https://vllm.internal/v1"),
        ]));
        assert_eq!(
            plan.specs,
            vec![ProviderSpec::OpenAi {
                api_key: "sk-openai".into(),
                base_url: Some("https://vllm.internal/v1".into()),
            }]
        );
        assert_eq!(plan.default_model(), "openai");
    }

    #[test]
    fn detection_order_puts_anthropic_first_by_default() {
        let plan = ProviderPlan::from_env_with(env(&[
            ("OPENAI_API_KEY", "a"),
            ("ANTHROPIC_API_KEY", "b"),
        ]));
        assert_eq!(plan.default_model(), "anthropic");
    }

    #[test]
    fn dadhichi_provider_overrides_the_default() {
        let plan = ProviderPlan::from_env_with(env(&[
            ("OPENAI_API_KEY", "a"),
            ("ANTHROPIC_API_KEY", "b"),
            ("DADHICHI_PROVIDER", "openai"),
        ]));
        assert_eq!(plan.default_model(), "openai");
    }

    #[test]
    fn forced_provider_ignored_when_not_configured() {
        // Asking for openai without a key falls back to the detected default.
        let plan = ProviderPlan::from_env_with(env(&[
            ("ANTHROPIC_API_KEY", "b"),
            ("DADHICHI_PROVIDER", "openai"),
        ]));
        assert_eq!(plan.default_model(), "anthropic");
    }

    #[test]
    fn dadhichi_provider_ollama_enables_it_without_host() {
        let plan = ProviderPlan::from_env_with(env(&[("DADHICHI_PROVIDER", "ollama")]));
        assert_eq!(plan.specs, vec![ProviderSpec::Ollama { host: None }]);
        assert_eq!(plan.default_model(), "ollama");
    }

    #[test]
    fn ollama_host_enables_local_provider() {
        let plan = ProviderPlan::from_env_with(env(&[("OLLAMA_HOST", "http://localhost:11434")]));
        assert_eq!(
            plan.specs,
            vec![ProviderSpec::Ollama {
                host: Some("http://localhost:11434".into())
            }]
        );
    }

    #[test]
    fn blank_values_are_treated_as_unset() {
        let plan = ProviderPlan::from_env_with(env(&[("ANTHROPIC_API_KEY", "   ")]));
        assert!(!plan.has_remote());
        assert_eq!(plan.default_model(), "mock");
    }

    #[test]
    fn force_mock_stays_offline_even_with_a_key() {
        let plan = ProviderPlan::from_env_with(env(&[
            ("ANTHROPIC_API_KEY", "b"),
            ("DADHICHI_PROVIDER", "mock"),
        ]));
        // The key still registers the provider, but mock stays the default.
        assert!(plan.has_remote());
        assert_eq!(plan.default_model(), "mock");
    }

    #[test]
    fn debug_redacts_secrets() {
        let spec = ProviderSpec::Anthropic {
            api_key: "sk-ant-supersecret".into(),
        };
        let shown = format!("{spec:?}");
        assert!(!shown.contains("supersecret"));
        assert!(shown.contains("redacted"));

        let plan = ProviderPlan::from_env_with(env(&[("OPENAI_API_KEY", "sk-leak-me")]));
        assert!(!format!("{plan:?}").contains("sk-leak-me"));
    }

    #[cfg(feature = "http")]
    #[test]
    fn build_router_registers_providers_and_default() {
        let plan = ProviderPlan::from_env_with(env(&[("ANTHROPIC_API_KEY", "k")]));
        let router = plan.build_router();
        assert!(router.provider("anthropic").is_some());
        assert!(router.provider("mock").is_some());

        // The default route (unknown model) lands on anthropic, not mock.
        let req = crate::CompletionRequest::new("does-not-match-any-id");
        let routed = router.route(&req).unwrap();
        assert_eq!(routed.id(), "anthropic");
    }

    #[cfg(feature = "http")]
    #[test]
    fn build_router_offline_uses_mock() {
        let plan = ProviderPlan::from_env_with(env(&[]));
        let router = plan.build_router();
        assert!(router.provider("mock").is_some());
        let req = crate::CompletionRequest::new("whatever");
        assert_eq!(router.route(&req).unwrap().id(), "mock");
    }
}
