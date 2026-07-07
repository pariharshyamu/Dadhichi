//! The reference agent, exercising the whole framework end to end.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use crate::memory::Tier;
use crate::plan::{Plan, Step};
use async_trait::async_trait;
use dadhichi_ai::{CompletionRequest, Message};

/// A minimal agent that plans three steps, calls the model to satisfy the goal,
/// records the exchange to memory, and reports a confident outcome.
///
/// It is intentionally small — its job is to demonstrate the contract that the
/// heavier specialised agents follow, and to be fully testable offline against
/// the [`MockProvider`](dadhichi_ai::MockProvider).
#[derive(Debug, Clone)]
pub struct ConversationalAgent {
    name: String,
    model: String,
}

impl Default for ConversationalAgent {
    fn default() -> Self {
        Self {
            name: "conversational-agent".into(),
            model: "mock".into(),
        }
    }
}

impl ConversationalAgent {
    /// Create an agent that routes its calls to `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            name: "conversational-agent".into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl Agent for ConversationalAgent {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "General-purpose assistant for open-ended questions and coding help."
    }

    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError> {
        // 1. Plan.
        ctx.emit("agent.status", serde_json::json!({ "status": "planning" }));
        let mut plan = Plan::new(goal)
            .step(Step::think("understand the goal"))
            .step(Step::think("consult the model"))
            .step(Step::think("summarise the answer"));
        ctx.emit_plan(&plan);

        // 2. Act: understand.
        let step0 = plan.steps[0].id;
        ctx.memory.remember(Tier::Working, format!("goal: {goal}"));
        plan.complete(step0);
        ctx.emit_plan(&plan);

        // 2b. Act: consult the model.
        ctx.emit("agent.status", serde_json::json!({ "status": "running" }));
        let request = CompletionRequest::new(&self.model)
            .message(Message::system(
                "You are a helpful coding assistant inside the Dadhichi IDE.",
            ))
            .message(Message::user(goal));

        let completion = ctx
            .models
            .complete(request)
            .await
            .map_err(|e| AgentError::Model(e.to_string()))?;

        ctx.emit(
            "agent.tokens",
            serde_json::json!({ "total": completion.usage.total() }),
        );
        // Surface the model's actual reply on the bus so a frontend can show it.
        // Without this the console only sees telemetry (status/plan/tokens) and
        // the answer — the whole point of the run — never reaches the screen.
        ctx.emit(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": completion.content }),
        );
        ctx.memory
            .remember(Tier::Conversation, completion.content.clone());
        // Keep the run inside the context window: compact the conversation once
        // its footprint crosses the policy threshold (a no-op when under budget).
        ctx.maybe_compact(&self.model).await;
        plan.complete(plan.steps[1].id);
        ctx.emit_plan(&plan);

        // 3. Reflect / summarise.
        plan.complete(plan.steps[2].id);
        ctx.emit_plan(&plan);
        let outcome = AgentOutcome {
            status: AgentStatus::Completed,
            summary: completion.content,
            confidence: 0.9,
            plan,
        };
        ctx.emit(
            "agent.status",
            serde_json::json!({ "status": "completed", "confidence": outcome.confidence }),
        );
        Ok(outcome)
    }
}
