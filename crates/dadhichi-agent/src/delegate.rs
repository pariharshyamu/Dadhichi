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
use dadhichi_git::{FileStatus, GitRepo};
use dadhichi_mcp::{
    BuildTool, DbQueryTool, FsGlobTool, FsGrepTool, FsListTool, FsReadTool, FsWriteTool, GrantSet,
    OverlayChange, OverlayStore, Permission, ScaffoldTool, StateError, StateStore, TerminalTool,
    TestRunnerTool, ToolRegistry, WorkspaceStore,
};
use std::path::{Path, PathBuf};
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

    /// Constrain the delegate to a coarse [`CapabilityMode`], replacing its
    /// grants. A convenient filter when the caller thinks in terms of
    /// read-only / read-write / execute rather than individual permissions.
    pub fn with_capability(mut self, mode: CapabilityMode) -> Self {
        self.grants = mode.grants();
        self
    }

    /// Whether the spec grants a capability.
    fn grants(&self, perm: Permission) -> bool {
        self.grants.allows(&[perm])
    }
}

/// A coarse filter on a delegate's powers, mirroring grok's subagent capability
/// modes. Each maps onto a concrete [`GrantSet`]; the delegate's tool palette is
/// then derived from those grants exactly as for a hand-built spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityMode {
    /// Read and search only — no file writes, no shell.
    ReadOnly,
    /// Read plus create/edit files. No shell.
    ReadWrite,
    /// Read plus run shell commands. No file writes.
    Execute,
    /// Full access: read, write, and execute.
    All,
}

impl CapabilityMode {
    /// The grants this mode confers.
    pub fn grants(self) -> GrantSet {
        match self {
            CapabilityMode::ReadOnly => GrantSet::from_iter([Permission::ReadWorkspace]),
            CapabilityMode::ReadWrite => {
                GrantSet::from_iter([Permission::ReadWorkspace, Permission::WriteWorkspace])
            }
            CapabilityMode::Execute => {
                GrantSet::from_iter([Permission::ReadWorkspace, Permission::RunCommands])
            }
            CapabilityMode::All => GrantSet::from_iter([
                Permission::ReadWorkspace,
                Permission::WriteWorkspace,
                Permission::RunCommands,
            ]),
        }
    }

    /// Parse the mode name (`read-only` / `read-write` / `execute` / `all`).
    pub fn parse(s: &str) -> Option<CapabilityMode> {
        Some(match s {
            "read-only" | "readonly" => CapabilityMode::ReadOnly,
            "read-write" | "readwrite" => CapabilityMode::ReadWrite,
            "execute" => CapabilityMode::Execute,
            "all" => CapabilityMode::All,
            _ => return None,
        })
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
            tools: Vec::new(),
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
    stage: Staging,
}

/// Where a delegate's staged work lives until it lands — an in-memory overlay,
/// or a real git worktree.
#[derive(Debug)]
enum Staging {
    /// Copy-on-write overlay over the base store; landing flushes it.
    Overlay(Arc<OverlayStore>),
    /// A linked git worktree; landing copies the changed files into the parent
    /// working tree and removes the worktree.
    Worktree(WorktreeStaging),
}

#[derive(Debug)]
struct WorktreeStaging {
    /// The parent repository / workspace root that changes land into.
    parent_root: PathBuf,
    /// The worktree checkout the delegate wrote to.
    worktree_root: PathBuf,
    /// The worktree's registered name (for pruning).
    name: String,
    /// The files the delegate changed, relative to the worktree root.
    changed: Vec<OverlayChange>,
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

    /// Land the staged changes onto the parent workspace, returning the number
    /// of files changed. For an overlay this flushes it; for a worktree it
    /// copies the changed files into the parent tree and removes the worktree.
    /// The caller commits the result.
    pub fn land(&self) -> Result<usize, StateError> {
        match &self.stage {
            Staging::Overlay(overlay) => overlay.flush(),
            Staging::Worktree(w) => land_worktree(w),
        }
    }

    /// Discard the staged work without landing it. A no-op for an overlay
    /// (dropping it is enough); for a worktree it removes the checkout so a
    /// rejected delegation leaves nothing behind.
    pub fn discard(&self) {
        if let Staging::Worktree(w) = &self.stage {
            if let Ok(repo) = GitRepo::open(&w.parent_root) {
                let _ = repo.prune_worktree(&w.name);
            }
        }
    }

    /// Whether there is anything staged to land.
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Land a worktree delegation: copy each changed file from the worktree into the
/// parent tree (or delete it), then prune the worktree.
fn land_worktree(w: &WorktreeStaging) -> Result<usize, StateError> {
    let io = |e: std::io::Error| StateError::Io(e.to_string());
    let mut landed = 0;
    for change in &w.changed {
        let dst = w.parent_root.join(&change.path);
        if change.deleted {
            let _ = std::fs::remove_file(&dst);
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::copy(w.worktree_root.join(&change.path), &dst).map_err(io)?;
        }
        landed += 1;
    }
    if let Ok(repo) = GitRepo::open(&w.parent_root) {
        let _ = repo.prune_worktree(&w.name);
    }
    Ok(landed)
}

/// How a delegate's file changes are isolated from the parent workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// A copy-on-write overlay over an in-memory or virtual base store. The
    /// default: zero-cost and works without a git repo.
    #[default]
    Overlay,
    /// A real linked git worktree — its own checkout on its own branch, so the
    /// delegate can build and run tests against real files without disturbing
    /// the parent tree. Requires the workspace to be a git repository.
    Worktree,
}

/// Runs sub-agents in isolation and reports what they changed.
#[derive(Debug)]
pub struct Delegator {
    models: Arc<ModelRouter>,
    bus: EventBus,
    threshold: f32,
    critic: Option<Arc<dyn Critic>>,
    isolation: Isolation,
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
            isolation: Isolation::Overlay,
        }
    }

    /// Choose how delegates are isolated ([`Overlay`](Isolation::Overlay) by
    /// default, or a real [`Worktree`](Isolation::Worktree)).
    pub fn with_isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = isolation;
        self
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

    /// Run `agent` on `task` in isolation and return a review to land or reject.
    /// The delegate's tool palette is built from `spec.grants`, so it gets
    /// exactly the powers the spec allows. Isolation is [`Overlay`] by default
    /// (writes staged in a copy-on-write overlay over `base`) or [`Worktree`]
    /// (writes go to a real git worktree at `cwd`; `base` is unused).
    ///
    /// [`Overlay`]: Isolation::Overlay
    /// [`Worktree`]: Isolation::Worktree
    pub async fn delegate(
        &self,
        agent: &dyn Agent,
        spec: &SubAgentSpec,
        task: &str,
        base: Arc<dyn StateStore>,
        cwd: impl Into<PathBuf>,
    ) -> Result<DelegationReview, AgentError> {
        let cwd: PathBuf = cwd.into();
        match self.isolation {
            Isolation::Overlay => self.delegate_overlay(agent, spec, task, base, cwd).await,
            Isolation::Worktree => self.delegate_worktree(agent, spec, task, cwd).await,
        }
    }

    async fn delegate_overlay(
        &self,
        agent: &dyn Agent,
        spec: &SubAgentSpec,
        task: &str,
        base: Arc<dyn StateStore>,
        cwd: PathBuf,
    ) -> Result<DelegationReview, AgentError> {
        let overlay = Arc::new(OverlayStore::new(base));
        let store: Arc<dyn StateStore> = overlay.clone();
        let tools = build_registry(spec, store, &cwd);

        self.announce_start(spec, task, Isolation::Overlay);
        let outcome = self.run_delegate(agent, spec, task, tools).await?;
        let changes = overlay.changes();
        Ok(self
            .review_and_announce(spec, task, outcome, changes, Staging::Overlay(overlay))
            .await)
    }

    async fn delegate_worktree(
        &self,
        agent: &dyn Agent,
        spec: &SubAgentSpec,
        task: &str,
        cwd: PathBuf,
    ) -> Result<DelegationReview, AgentError> {
        let repo = GitRepo::open(&cwd).map_err(|e| {
            AgentError::Tool(format!("worktree isolation requires a git repository: {e}"))
        })?;
        let safe: String = spec
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let name = format!("dadhichi-delegate-{safe}-{}", uuid::Uuid::new_v4().simple());
        let wt_root = cwd.join(".dadhichi").join("worktrees").join(&name);
        let wt = repo
            .add_worktree(&name, &wt_root)
            .map_err(|e| AgentError::Tool(format!("create worktree: {e}")))?;

        let store: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&wt.path));
        let tools = build_registry(spec, store, &wt.path);

        self.announce_start(spec, task, Isolation::Worktree);
        let outcome = self.run_delegate(agent, spec, task, tools).await?;
        let changes = worktree_changes(&wt.path);
        let stage = Staging::Worktree(WorktreeStaging {
            parent_root: cwd,
            worktree_root: wt.path,
            name,
            changed: changes.clone(),
        });
        Ok(self
            .review_and_announce(spec, task, outcome, changes, stage)
            .await)
    }

    fn announce_start(&self, spec: &SubAgentSpec, task: &str, isolation: Isolation) {
        self.bus.publish(Event::new(
            "agent.delegated",
            serde_json::json!({
                "subagent": spec.name,
                "task": task,
                "isolation": format!("{isolation:?}"),
            }),
        ));
    }

    async fn run_delegate(
        &self,
        agent: &dyn Agent,
        spec: &SubAgentSpec,
        task: &str,
        tools: Arc<ToolRegistry>,
    ) -> Result<AgentOutcome, AgentError> {
        let mut ctx = AgentContext::new(
            self.models.clone(),
            tools,
            spec.grants.clone(),
            self.bus.clone(),
        );
        agent.run(task, &mut ctx).await
    }

    /// Run the critic (if any), assemble the review, and publish the reviewed
    /// event — shared by both isolation paths.
    async fn review_and_announce(
        &self,
        spec: &SubAgentSpec,
        task: &str,
        outcome: AgentOutcome,
        changes: Vec<OverlayChange>,
        stage: Staging,
    ) -> DelegationReview {
        let verdict = match &self.critic {
            Some(critic) => Some(critic.review(task, &outcome.summary, &changes).await),
            None => None,
        };
        let review = DelegationReview {
            outcome,
            changes,
            threshold: self.threshold,
            verdict,
            stage,
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
        review
    }
}

/// Build a delegate's tool registry: filesystem tools bound to `store`, plus the
/// shell and full-stack dev tools (rooted at `cmd_cwd`) when the spec grants
/// `RunCommands`.
fn build_registry(
    spec: &SubAgentSpec,
    store: Arc<dyn StateStore>,
    cmd_cwd: &Path,
) -> Arc<ToolRegistry> {
    let tools = ToolRegistry::new();
    tools.register(Arc::new(FsReadTool::new(store.clone())));
    tools.register(Arc::new(FsListTool::new(store.clone())));
    tools.register(Arc::new(FsGrepTool::new(store.clone())));
    tools.register(Arc::new(FsGlobTool::new(store.clone())));
    if spec.grants(Permission::WriteWorkspace) {
        tools.register(Arc::new(FsWriteTool::new(store.clone())));
    }
    if spec.grants(Permission::RunCommands) {
        tools.register(Arc::new(TerminalTool::in_dir(cmd_cwd)));
        tools.register(Arc::new(ScaffoldTool::new(cmd_cwd)));
        tools.register(Arc::new(BuildTool::new(cmd_cwd)));
        tools.register(Arc::new(TestRunnerTool::new(cmd_cwd)));
        tools.register(Arc::new(DbQueryTool::new(cmd_cwd)));
    }
    Arc::new(tools)
}

/// The files a delegate changed in its worktree, from `git status`.
fn worktree_changes(worktree: &Path) -> Vec<OverlayChange> {
    match GitRepo::open(worktree).and_then(|r| r.status()) {
        Ok(entries) => entries
            .into_iter()
            .map(|e| OverlayChange {
                path: e.path,
                deleted: e.status == FileStatus::Deleted,
            })
            .collect(),
        Err(_) => Vec::new(),
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

    #[test]
    fn capability_modes_map_to_grants() {
        let ro = CapabilityMode::ReadOnly.grants();
        assert!(ro.allows(&[Permission::ReadWorkspace]));
        assert!(!ro.allows(&[Permission::WriteWorkspace]));
        assert!(!ro.allows(&[Permission::RunCommands]));

        let ex = CapabilityMode::Execute.grants();
        assert!(ex.allows(&[Permission::RunCommands]));
        assert!(!ex.allows(&[Permission::WriteWorkspace]));

        let all = CapabilityMode::All.grants();
        assert!(all.allows(&[Permission::WriteWorkspace, Permission::RunCommands]));
    }

    #[test]
    fn with_capability_overrides_spec_grants() {
        // A writer spec constrained to read-only loses its write grant.
        let spec = SubAgentSpec::writer("x").with_capability(CapabilityMode::ReadOnly);
        assert!(spec.grants(Permission::ReadWorkspace));
        assert!(!spec.grants(Permission::WriteWorkspace));
    }

    #[test]
    fn capability_mode_parses() {
        assert_eq!(
            CapabilityMode::parse("read-only"),
            Some(CapabilityMode::ReadOnly)
        );
        assert_eq!(CapabilityMode::parse("execute"), Some(CapabilityMode::Execute));
        assert_eq!(CapabilityMode::parse("all"), Some(CapabilityMode::All));
        assert_eq!(CapabilityMode::parse("nope"), None);
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
            stage: Staging::Overlay(overlay.clone()),
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

    #[test]
    fn worktree_land_copies_changes_and_prunes() {
        // A real repo with one commit, then a worktree the "delegate" writes to.
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "base").unwrap();
        repo.stage_all().unwrap();
        repo.commit("init", "T", "t@e.com").unwrap();

        let wt_path = dir.path().join(".dadhichi").join("worktrees").join("wt1");
        let wt = repo.add_worktree("wt1", &wt_path).unwrap();
        std::fs::write(wt.path.join("new.txt"), "hello").unwrap();

        // The change is detected from the worktree's git status.
        let changes = worktree_changes(&wt.path);
        assert!(changes.iter().any(|c| c.path == "new.txt" && !c.deleted));

        // Landing copies it into the parent tree and removes the worktree.
        let staging = WorktreeStaging {
            parent_root: dir.path().to_path_buf(),
            worktree_root: wt.path.clone(),
            name: "wt1".into(),
            changed: changes,
        };
        let n = land_worktree(&staging).unwrap();
        assert!(n >= 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hello"
        );
        assert!(
            !repo
                .list_worktrees()
                .unwrap()
                .contains(&"wt1".to_string())
        );
    }

    #[tokio::test]
    async fn worktree_isolation_requires_a_git_repo() {
        let (models, bus) = deps();
        let delegator = Delegator::new(models, bus).with_isolation(Isolation::Worktree);
        let agent = ReactAgent::new("mock");
        let spec = SubAgentSpec::for_role("code-agent").unwrap();
        let base: Arc<dyn StateStore> = Arc::new(MemStore::new());
        let non_git = tempfile::tempdir().unwrap();

        let res = delegator
            .delegate(&agent, &spec, "do it", base, non_git.path())
            .await;
        assert!(res.is_err(), "worktree isolation must fail outside a git repo");
    }
}
