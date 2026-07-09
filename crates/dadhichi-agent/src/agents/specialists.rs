//! Specialised agents: one role per software-engineering concern.
//!
//! A [`SpecialistAgent`] is a thin **role over the shared ReAct engine**, not a
//! separate agent implementation. Each role carries a persona and a domain
//! *playbook* (extra system-prompt guidance plus the tools it should reach for);
//! its `run` builds a [`ReactAgent`](crate::agents::ReactAgent) configured with
//! that role and drives the exact same tool-using reason-act loop the default
//! agent uses. This is deliberate: there is **one** coherent loop, so a
//! specialist and the react-agent never diverge in behaviour or conflict — a
//! specialist is just the react-agent wearing a hat.
//!
//! Named constructors give the roster the roadmap calls for — code, refactor,
//! test, review, docs, git, security, plus the full-stack roles frontend,
//! backend, and database. New specialists are one constructor away.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome};
use crate::agents::ReactAgent;
use async_trait::async_trait;

/// An agent specialised to one role — a persona + playbook over the ReAct engine.
#[derive(Debug, Clone)]
pub struct SpecialistAgent {
    name: String,
    description: String,
    /// The persona the ReAct agent adopts (e.g. "an expert frontend engineer").
    persona: String,
    /// Domain guidance and preferred tools, appended to the base system prompt.
    playbook: String,
    model: String,
}

impl SpecialistAgent {
    /// Build a specialist role. `description` is the action-oriented summary a
    /// delegating agent reads to pick a specialist; `persona` is the character
    /// the ReAct engine adopts; `playbook` is the domain guidance appended to the
    /// shared system prompt (workflow, preferred tools, gotchas).
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        persona: impl Into<String>,
        playbook: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            persona: persona.into(),
            playbook: playbook.into(),
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
            "Write and complete new code from a specification, with tests.",
            "an expert software engineer",
            "Role playbook (code):\n\
             - Inspect the existing code first (grep/glob/read) to match its conventions.\n\
             - Implement the change with correct, idiomatic code; keep edits small and focused.\n\
             - Add or update tests for what you change, then run the build/test tools to verify.\n\
             - Report exactly which files you changed and how you verified them.",
        )
    }

    /// Restructures existing code without changing behaviour.
    pub fn refactor() -> Self {
        Self::new(
            "refactor-agent",
            "Restructure existing code for clarity without changing its behaviour.",
            "a refactoring specialist",
            "Role playbook (refactor):\n\
             - Locate the code and read it fully before touching it.\n\
             - Apply one safe, behaviour-preserving refactoring at a time.\n\
             - Run the tests after each change to prove behaviour is unchanged.",
        )
    }

    /// Generates and strengthens tests.
    pub fn test() -> Self {
        Self::new(
            "test-agent",
            "Write thorough, deterministic tests covering edge cases.",
            "a testing specialist",
            "Role playbook (test):\n\
             - Identify the units under test and enumerate happy-path, error, and edge cases.\n\
             - Write deterministic tests (no time/network flakiness); run them and make them pass.\n\
             - Prefer the project's existing test framework and layout.",
        )
    }

    /// Reviews a change for correctness and style.
    pub fn review() -> Self {
        Self::new(
            "review-agent",
            "Review a change for correctness bugs and concrete improvements.",
            "a meticulous code reviewer",
            "Role playbook (review):\n\
             - Read the diff and the surrounding code; reason about correctness and edge cases.\n\
             - Note concrete, actionable issues with file:line references.\n\
             - This role is read-only: report findings, do not modify code.",
        )
    }

    /// Writes and updates documentation.
    pub fn docs() -> Self {
        Self::new(
            "docs-agent",
            "Write and update clear, accurate documentation for the code.",
            "a technical writer",
            "Role playbook (docs):\n\
             - Understand the subject by reading the code and existing docs.\n\
             - Write clear, accurate prose; keep examples runnable and in sync with the code.",
        )
    }

    /// Performs Git operations and authors commit messages.
    pub fn git() -> Self {
        Self::new(
            "git-agent",
            "Stage changes and author clear, conventional Git commits.",
            "a version-control specialist",
            "Role playbook (git):\n\
             - Inspect the working tree, group related changes, and write a clear conventional \
             commit message.\n\
             - Use the shell/terminal tool for git commands; never force-push or rewrite history \
             unless explicitly asked.",
        )
    }

    /// Audits code for security issues.
    pub fn security() -> Self {
        Self::new(
            "security-agent",
            "Audit code for vulnerabilities and propose safe remediations.",
            "a security auditor",
            "Role playbook (security):\n\
             - Map the attack surface, scan for common vulnerabilities (injection, authz, secrets, \
             unsafe deserialization), and assess severity.\n\
             - Recommend concrete, safe remediations; do not introduce new dependencies casually.",
        )
    }

    /// Builds frontend applications (React, Angular, plain HTML/JS).
    pub fn frontend() -> Self {
        Self::new(
            "frontend-agent",
            "Build and wire up frontend apps (React, Angular, HTML/JS) and their UI.",
            "an expert frontend engineer",
            "Role playbook (frontend):\n\
             - Scaffold with the requested framework (React+Vite, Angular, or plain HTML/JS) using \
             the scaffold tool, then install and run the dev/build to verify it compiles.\n\
             - Build components, state, routing, and styling; keep API calls in sync with the \
             backend's routes and types.\n\
             - Verify with the frontend build/test tools before reporting done.",
        )
    }

    /// Builds backend services and APIs.
    pub fn backend() -> Self {
        Self::new(
            "backend-agent",
            "Build backend services and REST/GraphQL APIs (Node, Python, Rust).",
            "an expert backend engineer",
            "Role playbook (backend):\n\
             - Scaffold the service (Node/Express, Python/FastAPI, or Rust/Axum), define routes, \
             handlers, and validation, and connect it to the database.\n\
             - Keep the API contract explicit and in sync with the frontend; read secrets from \
             env/config, never hard-code them.\n\
             - Run the service's build/test and exercise the endpoints to verify.",
        )
    }

    /// Designs schemas, writes migrations, and queries databases.
    pub fn database() -> Self {
        Self::new(
            "database-agent",
            "Design schemas, write and run migrations, and query databases (Postgres, SQLite).",
            "a database engineer",
            "Role playbook (database):\n\
             - Design a normalised schema, write migrations, and run them with the migrate tool.\n\
             - Use the db query tool to inspect and verify data rather than guessing.\n\
             - Prefer Postgres for relational apps and SQLite for local/simple ones; keep \
             credentials in env/config.",
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
        // A specialist IS the ReAct engine wearing a role: same tool-using loop,
        // same continuation/budget behaviour, just steered by this role's persona
        // and playbook. Building it here (rather than being a distinct agent type)
        // is what guarantees a specialist and the default react-agent can never
        // diverge or conflict.
        let agent = ReactAgent::new(&self.model)
            .as_role(&self.name, &self.persona)
            .with_playbook(&self.playbook);
        agent.run(goal, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentStatus;
    use crate::test_support::test_context;

    #[tokio::test]
    async fn specialists_have_distinct_identities_and_playbooks() {
        assert_eq!(SpecialistAgent::code().name(), "code-agent");
        assert_eq!(SpecialistAgent::security().name(), "security-agent");
        // The full-stack roles are present.
        assert_eq!(SpecialistAgent::frontend().name(), "frontend-agent");
        assert_eq!(SpecialistAgent::backend().name(), "backend-agent");
        assert_eq!(SpecialistAgent::database().name(), "database-agent");
        // Each role carries its own playbook.
        assert_ne!(
            SpecialistAgent::test().playbook,
            SpecialistAgent::docs().playbook
        );
        assert!(SpecialistAgent::frontend().playbook.contains("frontend"));
    }

    #[tokio::test]
    async fn specialist_runs_on_the_react_engine() {
        let (mut ctx, _bus) = test_context();
        let agent = SpecialistAgent::review();
        // The mock model replies in prose (no tool JSON), so the ReAct loop takes
        // the reply as its final answer and completes — proving a specialist now
        // drives the same tool-using engine rather than a bespoke one-shot path.
        let outcome = agent.run("review the auth module", &mut ctx).await.unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(!outcome.summary.is_empty());
    }
}
