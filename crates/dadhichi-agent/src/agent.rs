//! The agent abstraction: lifecycle, execution context, and the [`Agent`] trait.

use crate::compaction::{CompactionPolicy, CompactionReport, estimate_tokens};
use crate::memory::{Memory, Tier};
use crate::plan::Plan;
use async_trait::async_trait;
use dadhichi_ai::{CompletionRequest, Message, ModelRouter};
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

/// Live handles for a frontend to stop or steer a run in progress.
///
/// Cheap to clone (the run and the console hold the same handles). A default
/// control is inert: never cancelled, empty inbox — so contexts built without
/// a frontend behave exactly as before.
#[derive(Debug, Clone, Default)]
pub struct RunControl {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    inbox: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RunControl {
    /// A fresh, un-cancelled control.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the run to stop at its next loop iteration.
    pub fn stop(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a stop has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Queue a user message for injection into the run's next model turn —
    /// mid-run steering ("focus on the parser first", "skip the tests").
    pub fn say(&self, text: impl Into<String>) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push(text.into());
        }
    }

    /// Take every queued steering message (oldest first).
    pub fn drain_messages(&self) -> Vec<String> {
        self.inbox
            .lock()
            .map(|mut inbox| std::mem::take(&mut *inbox))
            .unwrap_or_default()
    }
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
    /// When to compact the conversation to stay inside the context window.
    pub compaction: CompactionPolicy,
    /// Stop/steer handles shared with the frontend driving this run.
    pub control: RunControl,
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
            compaction: CompactionPolicy::from_env(),
            control: RunControl::new(),
            bus,
        }
    }

    /// Override the compaction policy (window and trigger fraction), returning
    /// `self` for chaining. Handy for tests and for pinning a specific model's
    /// window.
    pub fn with_compaction(mut self, policy: CompactionPolicy) -> Self {
        self.compaction = policy;
        self
    }

    /// Emit a progress event on `topic`, correlated to this run.
    pub fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.bus
            .publish(Event::new(topic, payload).with_correlation(self.correlation_id));
    }

    /// Emit the current `plan` as an `agent.plan` snapshot (goal + steps with
    /// their done state), so a frontend can render a live checklist. Call it on
    /// plan creation and after each step completes.
    pub fn emit_plan(&self, plan: &Plan) {
        self.emit(
            "agent.plan",
            serde_json::to_value(plan).unwrap_or_else(|_| serde_json::json!({ "steps": [] })),
        );
    }

    /// A rough token estimate of everything currently in memory, across tiers.
    /// This is the footprint the compaction policy watches.
    pub fn memory_token_estimate(&self) -> usize {
        [Tier::Working, Tier::Conversation, Tier::LongTerm]
            .into_iter()
            .flat_map(|tier| self.memory.recall_tier(tier))
            .map(|item| estimate_tokens(&item.content))
            .sum()
    }

    /// Compact the conversation **if** its estimated footprint has crossed the
    /// policy's threshold. Asks `model` to summarise the [`Conversation`](
    /// Tier::Conversation) tier, collapses those turns into a single durable
    /// [`LongTerm`](Tier::LongTerm) summary, and emits `agent.compacted` with the
    /// before/after token estimates. Returns the report when it acted, `None`
    /// when compaction wasn't needed (or the summary call failed — the run then
    /// simply carries on uncompacted).
    ///
    /// Call it after appending model turns to memory: it is cheap when under
    /// budget (an estimate and a comparison) and only reaches for the model when
    /// the window is actually filling up.
    pub async fn maybe_compact(&mut self, model: &str) -> Option<CompactionReport> {
        let before = self.memory_token_estimate();
        if !self.compaction.should_compact(before) {
            return None;
        }
        let convo: Vec<String> = self
            .memory
            .recall_tier(Tier::Conversation)
            .iter()
            .map(|item| item.content.clone())
            .collect();
        if convo.is_empty() {
            return None;
        }

        let request = CompletionRequest::new(model)
            .message(Message::system(
                "You are compacting an agent's working memory. Summarise the conversation so far \
                 in a few sentences, preserving decisions, facts, file paths, and open questions. \
                 Omit pleasantries.",
            ))
            .message(Message::user(convo.join("\n")));
        let summary = self.models.complete(request).await.ok()?.content;

        // Collapse the conversation into one long-term summary.
        self.memory.summarise_conversation(|_| summary.clone());
        let after = self.memory_token_estimate();
        self.emit(
            "agent.compacted",
            serde_json::json!({
                "before_tokens": before,
                "after_tokens": after,
                "threshold": self.compaction.threshold_tokens(),
            }),
        );
        Some(CompactionReport {
            before_tokens: before,
            after_tokens: after,
            summary,
        })
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
            compaction: self.compaction,
            // Shared deliberately: stopping a run stops its delegates too.
            control: self.control.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_context;

    #[tokio::test]
    async fn maybe_compact_collapses_conversation_when_over_budget() {
        let (mut ctx, bus) = test_context();
        // A tiny window so a handful of turns crosses the 85% threshold.
        ctx.compaction = CompactionPolicy::new(100, 0.85);
        let mut sub = bus.subscribe_topic("agent.compacted");

        for i in 0..20 {
            ctx.memory.remember(
                Tier::Conversation,
                format!("turn {i}: {}", "lorem ipsum ".repeat(4)),
            );
        }
        let before = ctx.memory_token_estimate();
        assert!(before >= ctx.compaction.threshold_tokens());

        let report = ctx.maybe_compact("mock").await.expect("should compact");
        assert_eq!(report.before_tokens, before);
        // (The offline mock echoes its input, so it doesn't actually shrink the
        // text; a real model summariser does. We assert the structural collapse.)
        // The conversation collapsed into a single durable summary.
        assert!(ctx.memory.recall_tier(Tier::Conversation).is_empty());
        assert_eq!(ctx.memory.recall_tier(Tier::LongTerm).len(), 1);

        let event = sub.recv().await.unwrap();
        assert_eq!(event.topic.as_str(), "agent.compacted");
        assert_eq!(event.payload["before_tokens"], before);
    }

    #[tokio::test]
    async fn maybe_compact_is_a_noop_under_budget() {
        let (mut ctx, _bus) = test_context();
        // The default window is large; a short turn stays well under budget.
        ctx.memory.remember(Tier::Conversation, "a short turn");
        assert!(ctx.maybe_compact("mock").await.is_none());
        assert_eq!(ctx.memory.recall_tier(Tier::Conversation).len(), 1);
    }
}
