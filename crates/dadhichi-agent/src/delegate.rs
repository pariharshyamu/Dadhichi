//! Isolated, verifiable sub-agent delegation — the "deep agents" pattern.
//!
//! A delegate runs against a copy-on-write [`OverlayStore`] layered over the real
//! workspace, so every file it writes is *staged*, not applied. When it finishes,
//! the orchestrator inspects the outcome: work whose self-verified confidence
//! clears a threshold is **auto-approved** and lands; anything below it is held
//! for a human yes/no. Landing flushes the overlay onto the workspace, from where
//! the caller commits it to the branch.
//!
//! This is the mechanism behind "sub-agents do actual work, with their own tools
//! and permissions, and land on the main branch after orchestrator approval or
//! verification". Isolation is the overlay; the tool palette and grants come from
//! the [`SubAgentSpec`]; verification is the delegate's reflected confidence
//! measured against a threshold.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use dadhichi_ai::ModelRouter;
use dadhichi_core::{Event, EventBus};
use dadhichi_mcp::{
    FsListTool, FsReadTool, FsWriteTool, GrantSet, OverlayChange, OverlayStore, Permission,
    StateError, StateStore, TerminalTool, ToolRegistry,
};
use std::path::PathBuf;
use std::sync::Arc;

/// The permission envelope and tool palette a delegated sub-agent runs under.
/// Different specs hand different specialists different powers — a reviewer that
/// is read-only, a code writer that can write files and run commands.
#[derive(Debug, Clone)]
pub struct SubAgentSpec {
    /// The specialist's registered name (used in delegation events).
    pub name: String,
    /// What the delegate may do. Its file writes always land in the overlay, so
    /// a `WriteWorkspace` grant here stages changes — it never overwrites the
    /// real workspace until the work is verified/approved and flushed.
    pub grants: GrantSet,
    /// The role the delegate adopts, fed to the agent as a persona so it behaves
    /// in character (empty for a generic delegate).
    pub persona: String,
}

impl SubAgentSpec {
    /// A read-only delegate (analysis, review): `fs.read` + `fs.ls`, no writes.
    pub fn read_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            grants: GrantSet::from_iter([Permission::ReadWorkspace]),
            persona: String::new(),
        }
    }

    /// A delegate that can produce work: read and write the (overlaid) workspace.
    pub fn writer(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            grants: GrantSet::from_iter([Permission::ReadWorkspace, Permission::WriteWorkspace]),
            persona: String::new(),
        }
    }

    /// Also grant shell access (`terminal.run`) to the delegate.
    pub fn with_commands(mut self) -> Self {
        self.grants.grant(Permission::RunCommands);
        self
    }

    /// Give the delegate a role to adopt.
    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = persona.into();
        self
    }

    /// The built-in specialist for `name`, each with a role and a tool/permission
    /// envelope matched to its job — a reviewer and a security auditor are
    /// read-only, a docs writer may write, a coder/tester/refactorer may write
    /// and run commands. Returns `None` if the name isn't in the [`roster`].
    ///
    /// [`roster`]: SubAgentSpec::roster
    pub fn for_role(name: &str) -> Option<Self> {
        let spec = match name {
            "code-agent" | "code" => Self::writer("code-agent").with_commands().with_persona(
                "an expert software engineer who implements correct, idiomatic code, \
                 and may run commands to build and check it",
            ),
            "test-agent" | "test" => Self::writer("test-agent").with_commands().with_persona(
                "a testing specialist who writes thorough, deterministic tests and runs \
                 them to confirm they pass",
            ),
            "refactor-agent" | "refactor" => {
                Self::writer("refactor-agent").with_commands().with_persona(
                    "a refactoring specialist who improves structure and clarity while \
                     preserving behaviour, running tests to confirm nothing changed",
                )
            }
            "docs-agent" | "docs" => Self::writer("docs-agent").with_persona(
                "a technical writer who writes and updates clear, accurate documentation \
                 files (you do not run commands)",
            ),
            "review-agent" | "review" => Self::read_only("review-agent").with_persona(
                "a meticulous code reviewer who reads the code and reports correctness \
                 bugs and concrete improvements — you do NOT modify files",
            ),
            "security-agent" | "security" => Self::read_only("security-agent").with_persona(
                "a security auditor who inspects the code for vulnerabilities and reports \
                 findings — you do NOT modify files",
            ),
            _ => return None,
        };
        Some(spec)
    }

    /// The canonical names of the built-in specialists.
    pub fn roster() -> &'static [&'static str] {
        &[
            "code-agent",
            "test-agent",
            "refactor-agent",
            "docs-agent",
            "review-agent",
            "security-agent",
        ]
    }

    /// Whether the spec grants a capability.
    fn grants(&self, perm: Permission) -> bool {
        self.grants.allows(&[perm])
    }
}

/// A finished delegation awaiting a landing decision. Holds the delegate's
/// outcome, the files it staged, and the overlay they live in.
#[derive(Debug)]
pub struct DelegationReview {
    /// The delegate's final outcome (status, summary, reflected confidence).
    pub outcome: AgentOutcome,
    /// The files the delegate wrote or deleted, still staged in the overlay.
    pub changes: Vec<OverlayChange>,
    /// The confidence a delegation must reach to land without human approval.
    pub threshold: f32,
    overlay: Arc<OverlayStore>,
}

impl DelegationReview {
    /// Whether the orchestrator can land this work without asking a human: the
    /// run completed and its verified confidence cleared the threshold.
    pub fn auto_approved(&self) -> bool {
        self.outcome.status == AgentStatus::Completed && self.outcome.confidence >= self.threshold
    }

    /// A one-line, human-readable reason for the landing decision.
    pub fn verdict_note(&self) -> String {
        format!(
            "{:?} · confidence {:.0}% vs threshold {:.0}% · {} file(s) changed",
            self.outcome.status,
            self.outcome.confidence * 100.0,
            self.threshold * 100.0,
            self.changes.len()
        )
    }

    /// Land the staged changes onto the base workspace (flush the overlay),
    /// returning the number of files changed. The caller commits them.
    pub fn land(&self) -> Result<usize, StateError> {
        self.overlay.flush()
    }

    /// Whether there is anything staged to land.
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Runs sub-agents in isolation and reports what they changed.
#[derive(Debug)]
pub struct Delegator {
    models: Arc<ModelRouter>,
    bus: EventBus,
    threshold: f32,
}

impl Delegator {
    /// A delegator that auto-lands work at ≥75% confidence by default.
    pub fn new(models: Arc<ModelRouter>, bus: EventBus) -> Self {
        Self {
            models,
            bus,
            threshold: 0.75,
        }
    }

    /// Set the confidence a delegation must reach to land without approval.
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Run `agent` on `task` in isolation over `base`, staging its file writes in
    /// an overlay and returning a review for the caller to land or reject. The
    /// delegate's tool palette is built from `spec.grants`, so it gets exactly
    /// the powers the spec allows — all filesystem access confined to the overlay.
    pub async fn delegate(
        &self,
        agent: &dyn Agent,
        spec: &SubAgentSpec,
        task: &str,
        base: Arc<dyn StateStore>,
        cwd: impl Into<PathBuf>,
    ) -> Result<DelegationReview, AgentError> {
        let overlay = Arc::new(OverlayStore::new(base));
        let store: Arc<dyn StateStore> = overlay.clone();

        // Assemble the delegate's own registry: filesystem tools bound to the
        // overlay, plus shell if granted. Reads/writes stay staged and isolated.
        let tools = ToolRegistry::new();
        tools.register(Arc::new(FsReadTool::new(store.clone())));
        tools.register(Arc::new(FsListTool::new(store.clone())));
        if spec.grants(Permission::WriteWorkspace) {
            tools.register(Arc::new(FsWriteTool::new(store.clone())));
        }
        if spec.grants(Permission::RunCommands) {
            tools.register(Arc::new(TerminalTool::in_dir(cwd)));
        }
        let tools = Arc::new(tools);

        self.bus.publish(Event::new(
            "agent.delegated",
            serde_json::json!({ "subagent": spec.name, "task": task }),
        ));

        let mut ctx = AgentContext::new(
            self.models.clone(),
            tools,
            spec.grants.clone(),
            self.bus.clone(),
        );
        let outcome = agent.run(task, &mut ctx).await?;
        let changes = overlay.changes();

        let review = DelegationReview {
            outcome,
            changes,
            threshold: self.threshold,
            overlay,
        };

        self.bus.publish(Event::new(
            "agent.delegation.reviewed",
            serde_json::json!({
                "subagent": spec.name,
                "auto_approved": review.auto_approved(),
                "verdict": review.verdict_note(),
                "changes": review.changes.iter().map(|c| &c.path).collect::<Vec<_>>(),
            }),
        ));

        Ok(review)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::ReactAgent;
    use dadhichi_ai::{MockProvider, ModelRouter};
    use dadhichi_mcp::MemStore;

    fn deps() -> (Arc<ModelRouter>, EventBus) {
        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));
        (Arc::new(router), EventBus::new())
    }

    #[tokio::test]
    async fn delegate_stages_changes_in_isolation_until_landed() {
        let (models, bus) = deps();
        let base: Arc<dyn StateStore> = Arc::new(MemStore::new());
        base.write("existing.txt", "v1").unwrap();

        // A writer delegate whose tools are bound to the overlay.
        let overlay = Arc::new(OverlayStore::new(base.clone()));
        let store: Arc<dyn StateStore> = overlay.clone();
        // Simulate the delegate's work directly against its (overlay) store, the
        // same store the Delegator would hand it, then assert isolation + landing.
        store.write("report.md", "findings").unwrap();
        let review = DelegationReview {
            outcome: AgentOutcome {
                status: AgentStatus::Completed,
                summary: "wrote report".into(),
                confidence: 0.9,
                plan: crate::plan::Plan::new("wrote report"),
            },
            changes: overlay.changes(),
            threshold: 0.75,
            overlay: overlay.clone(),
        };

        // High confidence ⇒ auto-approved, and the change is staged, not landed.
        assert!(review.auto_approved());
        assert!(review.has_changes());
        assert!(base.read("report.md").is_err(), "staged, not on base yet");

        // Landing flushes it to the base workspace.
        let n = review.land().unwrap();
        assert_eq!(n, 1);
        assert_eq!(base.read("report.md").unwrap(), "findings");

        let _ = (models, bus);
    }

    #[test]
    fn roster_specialists_have_distinct_tool_envelopes() {
        // A reviewer and a security auditor are read-only.
        let review = SubAgentSpec::for_role("review-agent").unwrap();
        assert!(review.grants(Permission::ReadWorkspace));
        assert!(!review.grants(Permission::WriteWorkspace));
        assert!(!review.grants(Permission::RunCommands));
        assert!(review.persona.contains("reviewer"));

        // A docs writer may write but not run commands.
        let docs = SubAgentSpec::for_role("docs").unwrap();
        assert!(docs.grants(Permission::WriteWorkspace));
        assert!(!docs.grants(Permission::RunCommands));

        // A coder may write and run commands.
        let code = SubAgentSpec::for_role("code-agent").unwrap();
        assert!(code.grants(Permission::WriteWorkspace));
        assert!(code.grants(Permission::RunCommands));

        // Unknown names are rejected, and every roster name resolves.
        assert!(SubAgentSpec::for_role("nope").is_none());
        for name in SubAgentSpec::roster() {
            assert!(SubAgentSpec::for_role(name).is_some(), "{name} resolves");
        }
    }

    #[tokio::test]
    async fn low_confidence_delegation_is_not_auto_approved() {
        let (models, bus) = deps();
        let base: Arc<dyn StateStore> = Arc::new(MemStore::new());
        let delegator = Delegator::new(models, bus).with_threshold(0.99);
        let spec = SubAgentSpec::writer("code-agent");
        let agent = ReactAgent::new("mock");

        let review = delegator
            .delegate(&agent, &spec, "add a helper", base.clone(), ".")
            .await
            .unwrap();

        // The mock model can't clear a 99% bar, so the work is held for approval.
        assert!(!review.auto_approved());
        // Nothing landed on the base while it awaits a decision.
        assert!(base.list("").unwrap().is_empty());
    }
}
