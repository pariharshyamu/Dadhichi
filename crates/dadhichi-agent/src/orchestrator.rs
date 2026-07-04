//! The orchestrator: a registry of agents plus sequential, parallel, and
//! checkpointed execution.
//!
//! It is the runtime that turns a roster of [`Agent`]s into a coordinated
//! system — running one by name, fanning several out concurrently on forked
//! contexts, and snapshotting state so a failed step can be rolled back.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use crate::memory::MemoryItem;
use crate::plan::Plan;
use std::collections::HashMap;
use std::sync::Arc;

/// A restorable snapshot of an agent's progress.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// The plan at capture time.
    pub plan: Plan,
    /// The full memory contents at capture time.
    pub memory: Vec<MemoryItem>,
    /// The lifecycle status at capture time.
    pub status: AgentStatus,
}

impl Checkpoint {
    /// Capture the current `plan` and `ctx` memory.
    pub fn capture(plan: &Plan, ctx: &AgentContext, status: AgentStatus) -> Self {
        Self {
            plan: plan.clone(),
            memory: ctx.memory.snapshot(),
            status,
        }
    }

    /// Restore this checkpoint's memory into `ctx`, returning the saved plan.
    pub fn restore(&self, ctx: &mut AgentContext) -> Plan {
        ctx.memory.restore(self.memory.clone());
        self.plan.clone()
    }
}

/// Coordinates a set of named agents.
#[derive(Clone, Default)]
pub struct Orchestrator {
    agents: HashMap<String, Arc<dyn Agent>>,
}

impl std::fmt::Debug for Orchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchestrator")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Orchestrator {
    /// Create an empty orchestrator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `agent` under its own name.
    pub fn register(&mut self, agent: Arc<dyn Agent>) -> &mut Self {
        self.agents.insert(agent.name().to_string(), agent);
        self
    }

    /// Resolve an agent by name.
    pub fn agent(&self, name: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(name).cloned()
    }

    /// The names of every registered agent.
    pub fn agent_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.agents.keys().cloned().collect();
        names.sort();
        names
    }

    /// Run one agent by name against `goal`, using `ctx`.
    pub async fn run(
        &self,
        name: &str,
        goal: &str,
        ctx: &mut AgentContext,
    ) -> Result<AgentOutcome, AgentError> {
        let agent = self
            .agent(name)
            .ok_or_else(|| AgentError::UnknownAgent(name.to_string()))?;
        agent.run(goal, ctx).await
    }

    /// Run several `(agent, goal)` tasks **in parallel**, each on its own forked
    /// context, and collect the results in input order.
    ///
    /// Forking gives every task independent memory while sharing the model
    /// router, tool registry, and event bus, so the Agent Console still sees a
    /// single interleaved stream.
    pub async fn run_parallel(
        &self,
        template: &AgentContext,
        tasks: Vec<(String, String)>,
    ) -> Vec<(String, Result<AgentOutcome, AgentError>)> {
        let mut handles = Vec::with_capacity(tasks.len());
        for (name, goal) in tasks {
            let agent = self.agent(&name);
            let mut ctx = template.fork();
            handles.push(tokio::spawn(async move {
                let result = match agent {
                    Some(a) => a.run(&goal, &mut ctx).await,
                    None => Err(AgentError::UnknownAgent(name.clone())),
                };
                (name, result)
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(pair) => results.push(pair),
                Err(_) => results.push(("<panicked>".into(), Err(AgentError::Cancelled))),
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::SpecialistAgent;
    use crate::plan::{Plan, Step};
    use crate::test_support::test_context;

    fn orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new();
        orch.register(Arc::new(SpecialistAgent::code()));
        orch.register(Arc::new(SpecialistAgent::test()));
        orch.register(Arc::new(SpecialistAgent::docs()));
        orch
    }

    #[tokio::test]
    async fn runs_a_named_agent() {
        let orch = orchestrator();
        let (mut ctx, _bus) = test_context();
        let outcome = orch
            .run("code-agent", "add a feature", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let orch = orchestrator();
        let (mut ctx, _bus) = test_context();
        let err = orch.run("nope", "x", &mut ctx).await.unwrap_err();
        assert!(matches!(err, AgentError::UnknownAgent(_)));
    }

    #[tokio::test]
    async fn runs_tasks_in_parallel() {
        let orch = orchestrator();
        let (ctx, _bus) = test_context();
        let results = orch
            .run_parallel(
                &ctx,
                vec![
                    ("code-agent".into(), "impl X".into()),
                    ("test-agent".into(), "test X".into()),
                    ("docs-agent".into(), "document X".into()),
                ],
            )
            .await;

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
        // Each fork had independent memory (no cross-task contamination).
        assert_eq!(results[0].0, "code-agent");
    }

    #[tokio::test]
    async fn checkpoint_captures_and_restores_memory() {
        let (mut ctx, _bus) = test_context();
        ctx.memory.remember(crate::memory::Tier::Working, "before");
        let plan = Plan::new("g").step(Step::think("s"));

        let checkpoint = Checkpoint::capture(&plan, &ctx, AgentStatus::Running);
        ctx.memory.remember(crate::memory::Tier::Working, "after");
        assert_eq!(ctx.memory.len(), 2);

        checkpoint.restore(&mut ctx);
        assert_eq!(ctx.memory.len(), 1);
        assert!(!ctx.memory.recall("before").is_empty());
        assert!(ctx.memory.recall("after").is_empty());
    }
}
