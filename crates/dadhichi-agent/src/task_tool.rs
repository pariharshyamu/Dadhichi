//! The `task` tool: delegate a sub-task to a specialist agent with an isolated
//! context window.
//!
//! This is the "deep agent" delegation primitive. When an agent calls `task`, a
//! fresh [`AgentContext`] is built — new, empty memory and a new correlation id —
//! and the named specialist runs against *only* the supplied task description.
//! Nothing of the caller's working state leaks in, and only the specialist's
//! final [`AgentOutcome`](crate::AgentOutcome) summary flows back. That "context
//! quarantine" is what lets a long-running agent farm out chunky, self-contained
//! work (write tests, audit for vulns, draft docs) without polluting its own
//! context window with the delegate's intermediate reasoning.
//!
//! The tool is registered in the shared [`ToolRegistry`], so a model-driven
//! tool-calling loop can invoke it like any other tool, and the app also exposes
//! it as the `agent.spawn` command.

use crate::agent::AgentContext;
use crate::orchestrator::Orchestrator;
use async_trait::async_trait;
use dadhichi_ai::ModelRouter;
use dadhichi_core::{Event, EventBus};
use dadhichi_mcp::{GrantSet, Tool, ToolError, ToolRegistry, ToolResult, ToolSpec};
use std::sync::{Arc, Weak};

/// Delegates isolated sub-tasks to the specialists registered in an
/// [`Orchestrator`]. Holds everything needed to build a fresh context per call.
///
/// The tool registry is held as a [`Weak`] on purpose: this tool is itself
/// registered *in* that registry, so a strong handle would form a reference
/// cycle that never frees. It is upgraded per call; sub-agents run with whatever
/// tools are live at delegation time.
pub struct TaskTool {
    orchestrator: Arc<Orchestrator>,
    models: Arc<ModelRouter>,
    tools: Weak<ToolRegistry>,
    bus: EventBus,
    grants: GrantSet,
}

impl std::fmt::Debug for TaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTool")
            .field("sub_agents", &self.orchestrator.agent_names())
            .finish_non_exhaustive()
    }
}

impl TaskTool {
    /// The name this tool registers under.
    pub const NAME: &'static str = "task";

    /// Build a delegation tool over `orchestrator`. `grants` is the permission
    /// envelope every spawned sub-agent runs under (typically the same grants the
    /// top-level run holds).
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        models: Arc<ModelRouter>,
        tools: &Arc<ToolRegistry>,
        bus: EventBus,
        grants: GrantSet,
    ) -> Self {
        Self {
            orchestrator,
            models,
            tools: Arc::downgrade(tools),
            bus,
            grants,
        }
    }

    /// A bullet list of the delegable specialists, embedded in the tool
    /// description so the model knows which `subagent_type` values are valid.
    fn roster(&self) -> String {
        self.orchestrator
            .sub_agents()
            .into_iter()
            .map(|(name, desc)| format!("- {name}: {desc}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        let names: Vec<String> = self
            .orchestrator
            .sub_agents()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Delegate a self-contained sub-task to a specialist agent, which \
                 runs with an isolated context window and returns only its final \
                 summary. Use this for chunky, independent work so its \
                 intermediate reasoning stays out of your own context.\n\n\
                 Available subagent_type values:\n{}",
                self.roster()
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "subagent_type": {
                        "type": "string",
                        "description": "Which specialist to delegate to.",
                        "enum": names,
                    },
                    "description": {
                        "type": "string",
                        "description": "The complete, self-contained task for the sub-agent — it sees nothing else.",
                    }
                },
                "required": ["subagent_type", "description"]
            }),
            // Delegation itself is ungated; the sub-agent's own tool calls are
            // still checked against the grants it runs under.
            permissions: vec![],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let subagent_type = args
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `subagent_type`".into()))?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `description`".into()))?;

        // Fail fast on an unknown specialist, with the valid options.
        if self.orchestrator.agent(subagent_type).is_none() {
            return Err(ToolError::Execution(format!(
                "unknown subagent_type `{subagent_type}`; available: {}",
                self.orchestrator.agent_names().join(", ")
            )));
        }

        // A brand-new context: empty memory, fresh correlation id. This is the
        // isolation boundary — the sub-agent starts from nothing but the task.
        let tools = self
            .tools
            .upgrade()
            .ok_or_else(|| ToolError::Execution("tool registry no longer available".into()))?;
        let mut isolated = AgentContext::new(
            self.models.clone(),
            tools,
            self.grants.clone(),
            self.bus.clone(),
        );

        self.bus.publish(Event::new(
            "agent.delegated",
            serde_json::json!({ "subagent_type": subagent_type, "description": description }),
        ));

        let outcome = self
            .orchestrator
            .run(subagent_type, description, &mut isolated)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        self.bus.publish(Event::new(
            "agent.delegation.done",
            serde_json::json!({
                "subagent_type": subagent_type,
                "status": format!("{:?}", outcome.status),
                "confidence": outcome.confidence,
            }),
        ));

        Ok(serde_json::json!({
            "subagent_type": subagent_type,
            "status": format!("{:?}", outcome.status),
            "summary": outcome.summary,
            "confidence": outcome.confidence,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::SpecialistAgent;
    use dadhichi_ai::{MockProvider, ModelRouter};

    /// Returns the tool plus the registry `Arc` the caller must keep alive (the
    /// tool holds only a `Weak` to it).
    fn task_tool() -> (TaskTool, EventBus, Arc<ToolRegistry>) {
        let mut orch = Orchestrator::new();
        orch.register(Arc::new(SpecialistAgent::code()));
        orch.register(Arc::new(SpecialistAgent::test()));

        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));
        let bus = EventBus::new();
        let registry = Arc::new(ToolRegistry::new());
        let tool = TaskTool::new(
            Arc::new(orch),
            Arc::new(router),
            &registry,
            bus.clone(),
            GrantSet::none(),
        );
        (tool, bus, registry)
    }

    #[test]
    fn spec_lists_available_subagents() {
        let (tool, _bus, _reg) = task_tool();
        let spec = tool.spec();
        assert_eq!(spec.name, "task");
        assert!(spec.description.contains("code-agent"));
        assert!(spec.description.contains("test-agent"));
    }

    #[tokio::test]
    async fn delegates_and_returns_only_a_summary() {
        let (tool, bus, _reg) = task_tool();
        // Subscribe before invoking so the delegation events are delivered.
        let mut sub = bus.subscribe();

        let out = tool
            .invoke(serde_json::json!({
                "subagent_type": "code-agent",
                "description": "implement a ring buffer"
            }))
            .await
            .unwrap();
        assert_eq!(out["subagent_type"], "code-agent");
        assert_eq!(out["status"], "Completed");
        assert!(out["summary"].as_str().unwrap().contains("ring buffer"));

        // A delegation marker was published for the console.
        let mut saw_delegated = false;
        while let Ok(Some(ev)) = sub.try_recv() {
            if ev.topic.as_str() == "agent.delegated" {
                saw_delegated = true;
            }
        }
        assert!(saw_delegated);
    }

    #[tokio::test]
    async fn unknown_subagent_is_a_clear_error() {
        let (tool, _bus, _reg) = task_tool();
        let err = tool
            .invoke(serde_json::json!({
                "subagent_type": "nope-agent",
                "description": "x"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(err.to_string().contains("code-agent"));
    }

    #[tokio::test]
    async fn missing_arguments_are_rejected() {
        let (tool, _bus, _reg) = task_tool();
        let err = tool
            .invoke(serde_json::json!({ "description": "no type" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
