//! Cost accounting for model calls.
//!
//! Providers report [`Usage`](crate::types::Usage) in tokens; this module turns
//! that into money using a per-model price table. Costs drive the router's
//! cost-optimisation policy and the per-run figure the Agent Console displays.

use crate::types::Usage;
use std::collections::HashMap;

/// Price of a model, expressed per **million** tokens (the unit every major
/// provider quotes). Kept as `f64` USD; a fraction of a cent matters at scale.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelPricing {
    /// USD per million prompt (input) tokens.
    pub input_per_mtok: f64,
    /// USD per million completion (output) tokens.
    pub output_per_mtok: f64,
}

impl ModelPricing {
    /// A pricing entry.
    pub fn new(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
        }
    }

    /// A free (local) model — Ollama, llama.cpp, vLLM on your own hardware.
    pub const FREE: Self = Self {
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
    };

    /// Cost in USD of an exchange with the given token `usage`.
    pub fn cost_of(&self, usage: Usage) -> f64 {
        let per_tok = |mtok_price: f64, tokens: u32| (tokens as f64 / 1_000_000.0) * mtok_price;
        per_tok(self.input_per_mtok, usage.prompt_tokens)
            + per_tok(self.output_per_mtok, usage.completion_tokens)
    }
}

/// A registry mapping model ids to their pricing.
#[derive(Debug, Clone, Default)]
pub struct CostTable {
    prices: HashMap<String, ModelPricing>,
    /// Applied to models with no explicit entry (defaults to free/local).
    fallback: ModelPricing,
}

impl CostTable {
    /// An empty table whose unknown-model fallback is [`ModelPricing::FREE`].
    pub fn new() -> Self {
        Self {
            prices: HashMap::new(),
            fallback: ModelPricing::FREE,
        }
    }

    /// Register `pricing` for `model`, returning `self` for chaining.
    pub fn with(mut self, model: impl Into<String>, pricing: ModelPricing) -> Self {
        self.prices.insert(model.into(), pricing);
        self
    }

    /// Set the pricing used for models with no explicit entry.
    pub fn with_fallback(mut self, pricing: ModelPricing) -> Self {
        self.fallback = pricing;
        self
    }

    /// The pricing for `model`, or the fallback if none is registered.
    pub fn pricing(&self, model: &str) -> ModelPricing {
        self.prices.get(model).copied().unwrap_or(self.fallback)
    }

    /// Cost in USD of `usage` charged at `model`'s rate.
    pub fn cost(&self, model: &str, usage: Usage) -> f64 {
        self.pricing(model).cost_of(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_blended_cost() {
        // $3 / Mtok in, $15 / Mtok out.
        let pricing = ModelPricing::new(3.0, 15.0);
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
        };
        assert!((pricing.cost_of(usage) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn local_models_are_free() {
        let usage = Usage {
            prompt_tokens: 5_000_000,
            completion_tokens: 5_000_000,
        };
        assert_eq!(ModelPricing::FREE.cost_of(usage), 0.0);
    }

    #[test]
    fn table_falls_back_for_unknown_models() {
        let table = CostTable::new()
            .with("gpt-x", ModelPricing::new(2.5, 10.0))
            .with_fallback(ModelPricing::new(1.0, 1.0));
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
        };
        assert!((table.cost("gpt-x", usage) - 2.5).abs() < 1e-9);
        assert!((table.cost("mystery", usage) - 1.0).abs() < 1e-9);
    }
}
