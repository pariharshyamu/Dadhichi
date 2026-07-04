//! # dadhichi-agent
//!
//! The **agent framework**. An [`Agent`] pursues a natural-language goal by
//! decomposing it into a [`Plan`], invoking tools through the permission-gated
//! registry, consulting models via the router, and recording what it learns in
//! layered [`Memory`]. Progress is streamed as events onto the kernel bus so
//! the Agent Console can render the live plan, tool calls, tokens, and
//! confidence.
//!
//! Every specialised agent — code, refactor, test, review, git, security — is
//! just another implementation of the one [`Agent`] trait, so the orchestrator
//! treats them uniformly.
//!
//! ```
//! use dadhichi_agent::{Agent, AgentContext, ConversationalAgent};
//! use dadhichi_ai::{ModelRouter, MockProvider};
//! use dadhichi_core::EventBus;
//! use dadhichi_mcp::{GrantSet, ToolRegistry};
//! use std::sync::Arc;
//!
//! # async fn demo() {
//! let mut router = ModelRouter::new();
//! router.register(Arc::new(MockProvider::default()));
//!
//! let mut ctx = AgentContext::new(
//!     Arc::new(router),
//!     Arc::new(ToolRegistry::new()),
//!     GrantSet::none(),
//!     EventBus::new(),
//! );
//!
//! let agent = ConversationalAgent::default();
//! let outcome = agent.run("explain the borrow checker", &mut ctx).await.unwrap();
//! assert!(outcome.confidence > 0.5);
//! # }
//! ```

pub mod agent;
pub mod agents;
pub mod memory;
pub mod plan;
pub mod semantic;

pub use agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
pub use agents::ConversationalAgent;
pub use memory::{Memory, MemoryItem, Tier};
pub use plan::{Plan, Step};
pub use semantic::SemanticMemory;

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_ai::{MockProvider, ModelRouter};
    use dadhichi_core::EventBus;
    use dadhichi_mcp::{GrantSet, ToolRegistry};
    use std::sync::Arc;

    fn test_context(bus: EventBus) -> AgentContext {
        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));
        AgentContext::new(
            Arc::new(router),
            Arc::new(ToolRegistry::new()),
            GrantSet::none(),
            bus,
        )
    }

    #[tokio::test]
    async fn agent_completes_and_records_memory() {
        let bus = EventBus::new();
        let mut ctx = test_context(bus);

        let agent = ConversationalAgent::default();
        let outcome = agent.run("what is ownership", &mut ctx).await.unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert_eq!(outcome.plan.progress(), 1.0);
        assert!(outcome.summary.contains("what is ownership"));
        assert!(!ctx.memory.recall("ownership").is_empty());
    }

    #[tokio::test]
    async fn agent_emits_progress_events() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe_topic("agent.status");
        let mut ctx = test_context(bus);

        let agent = ConversationalAgent::default();
        tokio::spawn(async move {
            let _ = agent.run("goal", &mut ctx).await;
        });

        // First status event should be the planning transition.
        let event = sub.recv().await.unwrap();
        assert_eq!(event.payload["status"], "planning");
    }

    #[test]
    fn plan_progress_tracks_completion() {
        let mut plan = Plan::new("goal")
            .step(Step::think("a"))
            .step(Step::think("b"));
        assert_eq!(plan.progress(), 0.0);
        let id = plan.steps[0].id;
        assert!(plan.complete(id));
        assert_eq!(plan.progress(), 0.5);
    }
}
