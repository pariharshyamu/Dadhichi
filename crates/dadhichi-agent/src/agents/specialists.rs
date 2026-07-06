//! Specialised agents: one role per software-engineering concern.
//!
//! Rather than a bespoke type per role, a [`SpecialistAgent`] is parameterised
//! by a role (name, system prompt, plan skeleton). Named constructors give the
//! roster the roadmap calls for — code, refactor, test, review, docs, git,
//! security — each a real [`Agent`] that plans, consults the model, and reflects
//! on its own work to set confidence. New specialists are one constructor away.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use crate::memory::Tier;
use crate::plan::{Plan, Step};
use crate::verify::{HeuristicVerifier, Verifier};
use async_trait::async_trait;
use dadhichi_ai::{CompletionRequest, Message};

/// An agent specialised to one role.
#[derive(Debug, Clone)]
pub struct SpecialistAgent {
    name: String,
    description: String,
    system: String,
    steps: Vec<String>,
    model: String,
}

impl SpecialistAgent {
    /// Build a specialist from its role definition. `description` is the
    /// action-oriented summary a delegating agent reads to decide when to hand
    /// this specialist a task.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        system: impl Into<String>,
        steps: &[&str],
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            system: system.into(),
            steps: steps.iter().map(|s| s.to_string()).collect(),
            model: "mock".into(),
        }
    }

    /// Route this specialist's calls to `model`.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Writes and completes new code from a specification.
    pub fn code() -> Self {
        Self::new(
            "code-agent",
            "Write and complete new code from a specification.",
            "You are an expert software engineer. Implement the requested change with correct, idiomatic code.",
            &[
                "analyse the request",
                "design the implementation",
                "write the code",
                "self-review",
            ],
        )
    }

    /// Restructures existing code without changing behaviour.
    pub fn refactor() -> Self {
        Self::new(
            "refactor-agent",
            "Restructure existing code for clarity without changing its behaviour.",
            "You are a refactoring specialist. Improve structure and clarity while preserving behaviour.",
            &[
                "locate the code",
                "identify a safe refactoring",
                "apply it",
                "verify behaviour is unchanged",
            ],
        )
    }

    /// Generates and strengthens tests.
    pub fn test() -> Self {
        Self::new(
            "test-agent",
            "Write thorough, deterministic tests covering edge cases.",
            "You are a testing specialist. Write thorough, deterministic tests covering edge cases.",
            &[
                "identify units under test",
                "enumerate cases",
                "write tests",
                "check coverage",
            ],
        )
    }

    /// Reviews a change for correctness and style.
    pub fn review() -> Self {
        Self::new(
            "review-agent",
            "Review a change for correctness bugs and concrete improvements.",
            "You are a meticulous code reviewer. Find correctness bugs and suggest concrete improvements.",
            &[
                "read the diff",
                "reason about correctness",
                "note issues",
                "summarise the verdict",
            ],
        )
    }

    /// Writes and updates documentation.
    pub fn docs() -> Self {
        Self::new(
            "docs-agent",
            "Write and update clear, accurate documentation for the code.",
            "You are a technical writer. Produce clear, accurate documentation for the code.",
            &[
                "understand the subject",
                "outline the docs",
                "write the prose",
                "proof-read",
            ],
        )
    }

    /// Performs Git operations and authors commit messages.
    pub fn git() -> Self {
        Self::new(
            "git-agent",
            "Stage changes and author clear, conventional Git commits.",
            "You are a version-control specialist. Stage changes and write clear, conventional commits.",
            &[
                "inspect the working tree",
                "group related changes",
                "compose a message",
                "commit",
            ],
        )
    }

    /// Audits code for security issues.
    pub fn security() -> Self {
        Self::new(
            "security-agent",
            "Audit code for vulnerabilities and propose safe remediations.",
            "You are a security auditor. Identify vulnerabilities and propose safe remediations.",
            &[
                "map the attack surface",
                "scan for vulnerabilities",
                "assess severity",
                "recommend fixes",
            ],
        )
    }
}

#[async_trait]
impl Agent for SpecialistAgent {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError> {
        // 1. Plan.
        ctx.emit(
            "agent.status",
            serde_json::json!({ "agent": self.name, "status": "planning" }),
        );
        let mut plan = Plan::new(goal);
        for step in &self.steps {
            plan = plan.step(Step::think(step.clone()));
        }
        ctx.emit(
            "agent.plan",
            serde_json::json!({ "agent": self.name, "steps": plan.steps.len() }),
        );
        ctx.memory.remember(Tier::Working, format!("goal: {goal}"));

        // 2. Act: consult the model with the role's system prompt.
        ctx.emit(
            "agent.status",
            serde_json::json!({ "agent": self.name, "status": "running" }),
        );
        let request = CompletionRequest::new(&self.model)
            .message(Message::system(&self.system))
            .message(Message::user(goal));
        let completion = ctx
            .models
            .complete(request)
            .await
            .map_err(|e| AgentError::Model(e.to_string()))?;
        ctx.emit(
            "agent.tokens",
            serde_json::json!({ "agent": self.name, "total": completion.usage.total() }),
        );
        ctx.memory
            .remember(Tier::Conversation, completion.content.clone());

        // Mark every planned step complete (this reference agent acts in one
        // shot; a richer agent would complete steps as it goes).
        let ids: Vec<_> = plan.steps.iter().map(|s| s.id).collect();
        for id in ids {
            plan.complete(id);
        }

        // 3. Reflect: verify the work and derive a confidence score.
        let verdict = HeuristicVerifier.verify(goal, &completion.content, &plan);
        ctx.emit(
            "agent.status",
            serde_json::json!({
                "agent": self.name,
                "status": "completed",
                "confidence": verdict.confidence,
            }),
        );

        Ok(AgentOutcome {
            status: AgentStatus::Completed,
            summary: completion.content,
            confidence: verdict.confidence,
            plan,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_context;

    #[tokio::test]
    async fn specialists_have_distinct_identities() {
        assert_eq!(SpecialistAgent::code().name(), "code-agent");
        assert_eq!(SpecialistAgent::security().name(), "security-agent");
        assert_ne!(
            SpecialistAgent::test().system,
            SpecialistAgent::docs().system
        );
    }

    #[tokio::test]
    async fn specialist_runs_plans_and_reflects() {
        let (mut ctx, _bus) = test_context();
        let agent = SpecialistAgent::review();
        let outcome = agent.run("review the auth module", &mut ctx).await.unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert_eq!(outcome.plan.progress(), 1.0);
        // Confidence comes from the verifier, not a constant.
        assert!(outcome.confidence > 0.5 && outcome.confidence <= 1.0);
        assert!(outcome.summary.contains("review the auth module"));
    }
}
