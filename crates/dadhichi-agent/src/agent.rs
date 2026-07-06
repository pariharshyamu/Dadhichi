//! The agent abstraction: lifecycle, execution context, and the [`Agent`] trait.

use crate::memory::Memory;
use crate::plan::Plan;
use async_trait::async_trait;
use dadhichi_ai::ModelRouter;
use dadhichi_core::{Event, EventBus};
use dadhichi_mcp::{GrantSet, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

/// The lifecycle state of an agent run, surfaced live in the Agent Console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Not yet started.
    Idle,
    /// Decomposing the goal into a plan.
    Planning,
    /// Executing plan steps.
    Running,
    /// Suspended, resumable from a checkpoint.
    Paused,
    /// Finished successfully.
    Completed,
    /// Terminated by the user.
    Cancelled,
    /// Finished with an error.
    Failed,
}

/// The result of an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
    /// Terminal status.
    pub status: AgentStatus,
    /// A human-readable summary of what happened.
    pub summary: String,
    /// The agent's self-assessed confidence in `0.0..=1.0`.
    pub confidence: f32,
    /// The final plan (with completion state), for auditing.
    pub plan: Plan,
}

/// Errors that can abort an agent run.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The underlying model call failed.
    #[error("model error: {0}")]
    Model(String),
    /// A tool invocation failed.
    #[error("tool error: {0}")]
    Tool(String),
    /// No agent was registered under the requested name.
    #[error("unknown agent: {0}")]
    UnknownAgent(String),
    /// The run was cancelled cooperatively.
    #[error("cancelled")]
    Cancelled,
}

/// Everything an agent needs to do its work.
///
/// The context bundles the shared services (model router, tool registry, event
/// bus) with per-run state (memory, grants, correlation id). Agents emit
/// progress by publishing events on the bus tagged with `correlation_id`, which
/// the Agent Console subscribes to.
#[derive(Debug)]
pub struct AgentContext {
    /// Model router for LLM calls.
    pub models: Arc<ModelRouter>,
    /// Tool registry for capability invocation.
    pub tools: Arc<ToolRegistry>,
    /// Permissions this run is allowed to exercise.
    pub grants: GrantSet,
    /// The agent's memory across the run.
    pub memory: Memory,
    /// Correlates every event this run emits.
    pub correlation_id: Uuid,
    bus: EventBus,
}

impl AgentContext {
    /// Assemble a context from shared services and a fresh grant set.
    pub fn new(
        models: Arc<ModelRouter>,
        tools: Arc<ToolRegistry>,
        grants: GrantSet,
        bus: EventBus,
    ) -> Self {
        Self {
            models,
            tools,
            grants,
            memory: Memory::new(),
            correlation_id: Uuid::new_v4(),
            bus,
        }
    }

    /// Emit a progress event on `topic`, correlated to this run.
    pub fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.bus
            .publish(Event::new(topic, payload).with_correlation(self.correlation_id));
    }

    /// Create a sibling context that shares the services (model router, tool
    /// registry, event bus) and grants but gets fresh memory and a new
    /// correlation id. This is how the orchestrator runs several agents in
    /// parallel without them sharing mutable memory.
    pub fn fork(&self) -> AgentContext {
        AgentContext {
            models: self.models.clone(),
            tools: self.tools.clone(),
            grants: self.grants.clone(),
            memory: Memory::new(),
            correlation_id: Uuid::new_v4(),
            bus: self.bus.clone(),
        }
    }

    /// The event bus this context publishes on.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
}

/// An autonomous agent that pursues a natural-language goal.
///
/// Concrete agents (code, refactor, test, review, git, …) implement this one
/// trait. The orchestrator treats them uniformly, so new agent types are added
/// without changing the runtime.
#[async_trait]
pub trait Agent: Send + Sync {
    /// A stable, human-readable name, e.g. `"code-agent"`.
    fn name(&self) -> &str;

    /// A short, action-oriented description of what this agent is good at, used
    /// to help a delegating agent (or the user) decide when to hand it a task —
    /// e.g. "Write and complete new code from a specification.". Defaults to
    /// empty; concrete agents should override it to be delegable via `task`.
    fn description(&self) -> &str {
        ""
    }

    /// Pursue `goal`, driving `ctx` and returning an outcome.
    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError>;
}
