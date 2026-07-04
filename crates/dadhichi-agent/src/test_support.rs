//! Shared test helpers, compiled only under `cfg(test)`.

use crate::agent::AgentContext;
use dadhichi_ai::{MockProvider, ModelRouter};
use dadhichi_core::EventBus;
use dadhichi_mcp::{GrantSet, ToolRegistry};
use std::sync::Arc;

/// Build an offline [`AgentContext`] backed by the mock model, returning the
/// bus alongside so tests can observe emitted events.
pub fn test_context() -> (AgentContext, EventBus) {
    let mut router = ModelRouter::new();
    router.register(Arc::new(MockProvider::default()));
    let bus = EventBus::new();
    let ctx = AgentContext::new(
        Arc::new(router),
        Arc::new(ToolRegistry::new()),
        GrantSet::none(),
        bus.clone(),
    );
    (ctx, bus)
}
