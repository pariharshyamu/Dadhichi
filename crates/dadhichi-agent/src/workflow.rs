//! Workflow automation from natural language.
//!
//! A [`Workflow`] turns a request like *"Refactor authentication, write tests,
//! and update the documentation"* into an ordered list of [`WorkflowStep`]s,
//! each routed to the specialist best suited to it, then executes them through
//! the [`Orchestrator`](crate::Orchestrator). This is the delegation layer: the
//! workflow plans, and each specialist does its part.

use crate::agent::AgentContext;
use crate::orchestrator::Orchestrator;

/// One step of a workflow: a specialist and the sub-goal handed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    /// The name of the agent that will handle this step.
    pub agent: String,
    /// The natural-language sub-goal.
    pub goal: String,
}

/// An ordered plan of specialist steps.
#[derive(Debug, Clone, Default)]
pub struct Workflow {
    /// The steps, in execution order.
    pub steps: Vec<WorkflowStep>,
}

/// The result of one executed step.
#[derive(Debug, Clone)]
pub struct StepReport {
    /// The agent that ran.
    pub agent: String,
    /// The sub-goal it pursued.
    pub goal: String,
    /// Its answer summary (or the error text on failure).
    pub summary: String,
    /// Its self-assessed confidence.
    pub confidence: f32,
    /// Whether the step succeeded.
    pub ok: bool,
}

/// The result of a whole workflow run.
#[derive(Debug, Clone)]
pub struct WorkflowReport {
    /// Per-step results in order.
    pub steps: Vec<StepReport>,
}

impl WorkflowReport {
    /// The lowest step confidence — a workflow is only as trustworthy as its
    /// weakest step. `1.0` for an empty workflow.
    pub fn overall_confidence(&self) -> f32 {
        self.steps.iter().map(|s| s.confidence).fold(1.0, f32::min)
    }

    /// Whether every step succeeded.
    pub fn all_ok(&self) -> bool {
        self.steps.iter().all(|s| s.ok)
    }
}

impl Workflow {
    /// Parse a natural-language request into routed steps.
    ///
    /// The request is split into clauses (on commas and `then`/`and`), and each
    /// clause is routed to a specialist by keyword. Unmatched clauses fall to
    /// the code agent.
    pub fn parse(request: &str) -> Self {
        let steps = split_clauses(request)
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .map(|clause| WorkflowStep {
                agent: route(&clause).to_string(),
                goal: clause.trim().to_string(),
            })
            .collect();
        Self { steps }
    }

    /// Execute the workflow sequentially, forking a fresh context per step so
    /// steps do not share mutable memory. Returns a per-step report.
    pub async fn execute(
        &self,
        orchestrator: &Orchestrator,
        template: &AgentContext,
    ) -> WorkflowReport {
        let mut steps = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            let mut ctx = template.fork();
            let report = match orchestrator.run(&step.agent, &step.goal, &mut ctx).await {
                Ok(outcome) => StepReport {
                    agent: step.agent.clone(),
                    goal: step.goal.clone(),
                    summary: outcome.summary,
                    confidence: outcome.confidence,
                    ok: true,
                },
                Err(err) => StepReport {
                    agent: step.agent.clone(),
                    goal: step.goal.clone(),
                    summary: err.to_string(),
                    confidence: 0.0,
                    ok: false,
                },
            };
            steps.push(report);
        }
        WorkflowReport { steps }
    }
}

/// Split a request into clauses on commas and the connectives `then`/`and`.
fn split_clauses(request: &str) -> Vec<String> {
    request
        .split(',')
        .flat_map(|part| part.split(" then "))
        .flat_map(|part| part.split(" and "))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Route a clause to a specialist agent name by keyword.
fn route(clause: &str) -> &'static str {
    let c = clause.to_lowercase();
    if c.contains("refactor") {
        "refactor-agent"
    } else if c.contains("test") {
        "test-agent"
    } else if c.contains("document") || c.contains("docs") || c.contains("readme") {
        "docs-agent"
    } else if c.contains("review") {
        "review-agent"
    } else if c.contains("secur") || c.contains("vulnerab") || c.contains("audit") {
        "security-agent"
    } else if c.contains("commit")
        || c.contains("pull request")
        || c.contains(" pr")
        || c.contains("push")
        || c.contains("git")
    {
        "git-agent"
    } else {
        "code-agent"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::SpecialistAgent;
    use crate::test_support::test_context;
    use std::sync::Arc;

    fn full_orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new();
        for agent in [
            SpecialistAgent::code(),
            SpecialistAgent::refactor(),
            SpecialistAgent::test(),
            SpecialistAgent::docs(),
            SpecialistAgent::review(),
            SpecialistAgent::git(),
            SpecialistAgent::security(),
        ] {
            orch.register(Arc::new(agent));
        }
        orch
    }

    #[test]
    fn parses_and_routes_a_request() {
        let wf = Workflow::parse(
            "Refactor authentication, write tests, update the documentation, and open a pull request",
        );
        let agents: Vec<_> = wf.steps.iter().map(|s| s.agent.as_str()).collect();
        assert_eq!(
            agents,
            vec!["refactor-agent", "test-agent", "docs-agent", "git-agent"]
        );
    }

    #[tokio::test]
    async fn executes_the_whole_workflow() {
        let orch = full_orchestrator();
        let (ctx, _bus) = test_context();
        let wf = Workflow::parse("Refactor auth and write tests and audit for security issues");

        let report = wf.execute(&orch, &ctx).await;
        assert_eq!(report.steps.len(), 3);
        assert!(report.all_ok());
        assert!(report.overall_confidence() > 0.0);
        assert_eq!(report.steps[2].agent, "security-agent");
    }

    #[tokio::test]
    async fn a_missing_specialist_fails_only_its_step() {
        // An orchestrator without a docs agent: the docs step fails, others pass.
        let mut orch = Orchestrator::new();
        orch.register(Arc::new(SpecialistAgent::code()));
        let (ctx, _bus) = test_context();

        let wf = Workflow::parse("implement the parser, then update the documentation");
        let report = wf.execute(&orch, &ctx).await;
        assert!(report.steps[0].ok, "code step ran");
        assert!(!report.steps[1].ok, "docs step failed (no docs agent)");
        assert!(!report.all_ok());
    }
}
