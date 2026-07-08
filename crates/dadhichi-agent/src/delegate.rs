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
use async_trait::async_trait;
use dadhichi_ai::{CompletionRequest, Message, ModelRouter};
use dadhichi_core::{Event, EventBus};
use dadhichi_mcp::{
    FsGlobTool, FsGrepTool, FsListTool, FsReadTool, FsWriteTool, GrantSet, OverlayChange,
    OverlayStore, Permission, StateError, StateStore, TerminalTool, ToolRegistry,
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
    /// Names of skills the delegate equips — resolved from the skill library by
    /// the runtime, which folds their instructions into the delegate's prompt.
    /// This is how a spec pulls in prebuilt, reusable capabilities.
    pub skills: Vec<String>,
}

impl SubAgentSpec {
    /// A read-only delegate (analysis, review): `fs.read` + `fs.ls`, no writes.
    pub fn read_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            grants: GrantSet::from_iter([Permission::ReadWorkspace]),
            persona: String::new(),
            skills: Vec::new(),
        }
    }

    /// A delegate that can produce work: read and write the (overlaid) workspace.
    pub fn writer(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            grants: GrantSet::from_iter([Permission::ReadWorkspace, Permission::WriteWorkspace]),
            persona: String::new(),
            skills: Vec::new(),
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

    /// Equip the delegate with named skills from the library.
    pub fn with_skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.skills = skills.into_iter().map(Into::into).collect();
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
            "code-agent" | "code" => Self::writer("code-agent")
                .with_commands()
                .with_persona(
                    "an expert software engineer who implements correct, idiomatic code, \
                     and may run commands to build and check it",
                )
                .with_skills(["implement"]),
            "test-agent" | "test" => Self::writer("test-agent")
                .with_commands()
                .with_persona(
                    "a testing specialist who writes thorough, deterministic tests and runs \
                     them to confirm they pass",
                )
                .with_skills(["author-tests"]),
            "refactor-agent" | "refactor" => Self::writer("refactor-agent")
                .with_commands()
                .with_persona(
                    "a refactoring specialist who improves structure and clarity while \
                     preserving behaviour, running tests to confirm nothing changed",
                )
                .with_skills(["implement"]),
            "docs-agent" | "docs" => Self::writer("docs-agent")
                .with_persona(
                    "a technical writer who writes and updates clear, accurate documentation \
                     files (you do not run commands)",
                )
                .with_skills(["explain"]),
            "review-agent" | "review" => Self::read_only("review-agent")
                .with_persona(
                    "a meticulous code reviewer who reads the code and reports correctness \
                     bugs and concrete improvements — you do NOT modify files",
                )
                .with_skills(["code-review"]),
            "security-agent" | "security" => Self::read_only("security-agent")
                .with_persona(
                    "a security auditor who inspects the code for vulnerabilities and reports \
                     findings — you do NOT modify files",
                )
                .with_skills(["security-audit"]),
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

/// A verdict from an orchestrator-side critic reviewing a delegate's work — a
/// second opinion, independent of the delegate's own self-assessment.
#[derive(Debug, Clone)]
pub struct CriticVerdict {
    /// Confidence that the work satisfies the task, `0.0..=1.0`.
    pub confidence: f32,
    /// A one-line justification for the score.
    pub notes: String,
}

/// Independently judges a delegate's staged work before it lands. The default
/// signal is the delegate's own reflected confidence; a [`Critic`] replaces that
/// with a separate review, so the orchestrator isn't trusting the worker to
/// grade its own homework.
#[async_trait]
pub trait Critic: Send + Sync + std::fmt::Debug {
    /// Score the delegate's `summary` and staged `changes` against the `task`.
    async fn review(&self, task: &str, summary: &str, changes: &[OverlayChange]) -> CriticVerdict;
}

/// A critic that asks a model to review the delegate's work and return a score.
#[derive(Debug)]
pub struct ModelCritic {
    models: Arc<ModelRouter>,
    model: String,
}

impl ModelCritic {
    /// A critic that routes its review to `model`.
    pub fn new(models: Arc<ModelRouter>, model: impl Into<String>) -> Self {
        Self {
            models,
            model: model.into(),
        }
    }

    /// Pull a `{ "confidence": .., "notes": .. }` object out of a model reply.
    fn parse(text: &str) -> Option<CriticVerdict> {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        if end < start {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
        let confidence = value.get("confidence").and_then(|c| c.as_f64())? as f32;
        let notes = value
            .get("notes")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        Some(CriticVerdict {
            confidence: confidence.clamp(0.0, 1.0),
            notes,
        })
    }
}

#[async_trait]
impl Critic for ModelCritic {
    async fn review(&self, task: &str, summary: &str, changes: &[OverlayChange]) -> CriticVerdict {
        let files = if changes.is_empty() {
            "(no files changed)".to_string()
        } else {
            changes
                .iter()
                .map(|c| format!("{} {}", if c.deleted { "D" } else { "M" }, c.path))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let system = "You are a strict reviewer judging whether a sub-agent's work satisfies its \
             assigned task. Reply with EXACTLY ONE JSON object and nothing else: \
             {\"confidence\": <number 0.0-1.0>, \"notes\": \"<one short line>\"}. Be skeptical: \
             score low when the work is empty, off-task, or claims success without changing the \
             files the task requires.";
        let user = format!(
            "Task:\n{task}\n\nSub-agent's summary:\n{summary}\n\nFiles it staged:\n{files}"
        );
        let request = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::system(system), Message::user(user)],
            params: Default::default(),
        };
        match self.models.complete(request).await {
            Ok(completion) => Self::parse(&completion.content).unwrap_or(CriticVerdict {
                confidence: 0.5,
                notes: "critic reply was not parseable JSON".into(),
            }),
            Err(err) => CriticVerdict {
                confidence: 0.0,
                notes: format!("critic model error: {err}"),
            },
        }
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
    /// The orchestrator-side critic's verdict, when a [`Critic`] reviewed the
    /// work. When present it, not the delegate's self-assessment, drives the gate.
    pub verdict: Option<CriticVerdict>,
    overlay: Arc<OverlayStore>,
}

impl DelegationReview {
    /// The confidence the landing decision is based on: the critic's if one
    /// reviewed the work, otherwise the delegate's own reflected confidence.
    pub fn confidence(&self) -> f32 {
        match &self.verdict {
            Some(v) => v.confidence,
            None => self.outcome.confidence,
        }
    }

    /// Whether the orchestrator can land this work without asking a human: the
    /// run completed and the governing confidence cleared the threshold.
    pub fn auto_approved(&self) -> bool {
        self.outcome.status == AgentStatus::Completed && self.confidence() >= self.threshold
    }

    /// A one-line, human-readable reason for the landing decision.
    pub fn verdict_note(&self) -> String {
        let source = if self.verdict.is_some() {
            "critic"
        } else {
            "self"
        };
        let notes = self
            .verdict
            .as_ref()
            .map(|v| format!(" · {}", v.notes))
            .unwrap_or_default();
        format!(
            "{:?} · {source} confidence {:.0}% vs threshold {:.0}% · {} file(s) changed{}",
            self.outcome.status,
            self.confidence() * 100.0,
            self.threshold * 100.0,
            self.changes.len(),
            notes,
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
    critic: Option<Arc<dyn Critic>>,
}

impl Delegator {
    /// A delegator that auto-lands work at ≥75% confidence by default, judged by
    /// the delegate's own reflection unless a [`Critic`] is attached.
    pub fn new(models: Arc<ModelRouter>, bus: EventBus) -> Self {
        Self {
            models,
            bus,
            threshold: 0.75,
            critic: None,
        }
    }

    /// Set the confidence a delegation must reach to land without approval.
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Attach an orchestrator-side critic that independently reviews each
    /// delegate's work; its verdict, not the delegate's self-assessment, then
    /// governs whether the work auto-lands.
    pub fn with_critic(mut self, critic: Arc<dyn Critic>) -> Self {
        self.critic = Some(critic);
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
        // Read-only search tools every specialist can use to explore the code.
        tools.register(Arc::new(FsGrepTool::new(store.clone())));
        tools.register(Arc::new(FsGlobTool::new(store.clone())));
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

        // Orchestrator-side verification: an attached critic reviews the staged
        // work and its verdict governs the gate, so the delegate doesn't grade
        // its own homework.
        let verdict = match &self.critic {
            Some(critic) => Some(critic.review(task, &outcome.summary, &changes).await),
            None => None,
        };

        let review = DelegationReview {
            outcome,
            changes,
            threshold: self.threshold,
            verdict,
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
            verdict: None,
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

        // A coder may write and run commands, and equips the `implement` skill.
        let code = SubAgentSpec::for_role("code-agent").unwrap();
        assert!(code.grants(Permission::WriteWorkspace));
        assert!(code.grants(Permission::RunCommands));
        assert_eq!(code.skills, vec!["implement".to_string()]);
        assert_eq!(
            SubAgentSpec::for_role("review").unwrap().skills,
            vec!["code-review".to_string()]
        );

        // Unknown names are rejected, and every roster name resolves.
        assert!(SubAgentSpec::for_role("nope").is_none());
        for name in SubAgentSpec::roster() {
            assert!(SubAgentSpec::for_role(name).is_some(), "{name} resolves");
        }
    }

    #[tokio::test]
    async fn a_critic_verdict_overrides_the_delegates_self_confidence() {
        // A critic that always distrusts the work.
        #[derive(Debug)]
        struct HarshCritic;
        #[async_trait]
        impl Critic for HarshCritic {
            async fn review(&self, _t: &str, _s: &str, _c: &[OverlayChange]) -> CriticVerdict {
                CriticVerdict {
                    confidence: 0.1,
                    notes: "unconvinced".into(),
                }
            }
        }

        let (models, bus) = deps();
        let base: Arc<dyn StateStore> = Arc::new(MemStore::new());
        // Low threshold the delegate's own 0.9 confidence would clear...
        let delegator = Delegator::new(models, bus)
            .with_threshold(0.5)
            .with_critic(Arc::new(HarshCritic));
        let spec = SubAgentSpec::writer("code-agent");
        let agent = ReactAgent::new("mock");

        let review = delegator
            .delegate(&agent, &spec, "do work", base, ".")
            .await
            .unwrap();

        // ...but the critic's 0.1 governs, so it is held for approval.
        assert_eq!(review.confidence(), 0.1);
        assert!(!review.auto_approved());
        assert!(review.verdict_note().contains("critic"));
        assert!(review.verdict_note().contains("unconvinced"));
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
