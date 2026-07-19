//! # dadhichi-app
//!
//! The **application controller** — the seam where the standalone subsystems
//! become one running IDE. It boots the [kernel](dadhichi_core::Kernel),
//! registers real commands that spawn agents and re-index the workspace, and
//! bridges the kernel event bus into the [UI view-models](dadhichi_ui::App). A
//! frontend (the TUI, or a future GPU shell) drives it through three calls:
//!
//! - [`AppController::dispatch`] — run a command by name (what the palette does).
//! - [`AppController::pump`] — drain pending bus events into the UI each frame.
//! - [`AppController::ui`] / [`AppController::ui_mut`] — read/steer view state.
//!
//! This is the vertical slice that ties the phases together: a palette selection
//! dispatches a kernel command, which runs an agent, whose progress streams as
//! events back into the Agent Console panel — all through the same bus.

use dadhichi_agent::{
    Agent, AgentContext, ClaudeCodeAgent, DelegationReview, Delegator, MemoryRecallTool,
    MemoryWriteTool, ModelCritic, Orchestrator, ReactAgent, SharedMemory, SpecialistAgent,
    SubAgentSpec, TaskTool, agents::ConversationalAgent, session, shared_memory,
};
use dadhichi_ai::{ModelRouter, ProviderPlan};
use dadhichi_core::{Command, Event, Kernel, KernelError, RecvError, Subscription};
use dadhichi_git::GitRepo;
use dadhichi_index::Indexer;
use dadhichi_index::store::SqliteSymbolStore;
use dadhichi_mcp::{
    ApprovalPolicy, BuildTool, DbQueryTool, EchoTool, FsGlobTool, FsGrepTool, FsListTool,
    FsReadTool, FsWriteTool, GrantSet, McpConnection, McpConnections, McpServersConfig, Permission,
    PermissionMode, ScaffoldTool, StateStore, TerminalTool, TestRunnerTool, ToolRegistry,
    WorkspaceStore, connect_servers, connector,
};
use dadhichi_security::{SecretResolver, Vault, VaultData};
use dadhichi_skill::{
    SharedSkills, Skill, SkillAgent, SkillRegistry, SkillSpec, SkillTools, SkillWatchGuard, shared,
    watch_skills,
};
use dadhichi_lsp::LspManager;
use dadhichi_ui::{App, McpEntry, PaletteAction, SkillEntry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub mod approval;
mod htmlcheck;
pub use approval::{BusApprover, PendingApprovals};
// Re-exported so a frontend can answer prompts without depending on dadhichi-mcp.
pub use dadhichi_mcp::Decision;

/// Owns the live IDE state and mediates between the frontend and the kernel.
pub struct AppController {
    kernel: Kernel,
    ui: App,
    events: Subscription,
    store: Arc<SqliteSymbolStore>,
    skills: SharedSkills,
    /// Keeps the skill-manifest file watch alive; dropping it stops watching.
    _skill_watch: Option<SkillWatchGuard>,
    /// The declared MCP servers, for the `@` palette and `mcp.list`. Shared and
    /// mutable so `mcp.add` (connecting a built-in connector at runtime) shows up
    /// in the palette without a restart.
    mcp_config: Arc<Mutex<McpServersConfig>>,
    /// The live MCP connections, shared with the `mcp.*` command handlers so
    /// `mcp.connect`/`mcp.disconnect` can mutate the same set the palette reads.
    /// Holds each connection alive; disconnecting drops it and unregisters its
    /// tools.
    mcp: Arc<Mutex<McpConnections>>,
    /// The permission-gated tool registry (built-ins + bridged MCP tools). Held
    /// so a frontend can enumerate capabilities and the approval policy applies
    /// uniformly.
    tools: Arc<ToolRegistry>,
    /// Tool calls parked awaiting the user's `y/n` approval, keyed by request id.
    /// The `BusApprover` inserts a waiter here on interrupt; `resolve_approval`
    /// fires it when the frontend answers.
    approvals: PendingApprovals,
    /// The model router, for spawning delegated sub-agents and their critic.
    router: Arc<ModelRouter>,
    /// The workspace root — the delegation base store and git repo.
    root: PathBuf,
    /// The default model id delegated agents and the critic run under.
    model_id: String,
    /// The model the next `agent.run` executes under — switchable at runtime
    /// (the GUI's model picker writes here).
    current_model: Arc<std::sync::RwLock<String>>,
    /// A delegated sub-agent's staged work held pending the user's land/discard
    /// decision. `Some` while the Review panel is up; `resolve_delegation` takes it.
    pending_delegation: Arc<Mutex<Option<PendingDelegation>>>,
    /// Language servers for completion (and their pushed diagnostics), spawned
    /// lazily per language.
    lsp: Arc<LspManager>,
}

/// A delegation held between its review and the user's land/discard decision:
/// the reviewed work plus the metadata needed to attribute the commit.
struct PendingDelegation {
    subagent: String,
    task: String,
    review: DelegationReview,
}

/// Build the resolver for `${...}` secrets in `mcp.json`: `env:NAME` (and bare
/// names) from the process environment, and `vault:NAME` from the encrypted
/// credential vault.
///
/// The vault lives at `$DADHICHI_VAULT` (default `~/.dadhichi/vault.json`) and is
/// unlocked with `$DADHICHI_VAULT_PASSPHRASE`. Without a passphrase, or if the
/// file is absent, only `env:` references resolve — so tokens never have to sit
/// in plaintext environment variables once the vault is set up.
fn mcp_secret_resolver() -> Arc<SecretResolver> {
    let passphrase = std::env::var("DADHICHI_VAULT_PASSPHRASE")
        .ok()
        .filter(|p| !p.is_empty());
    let resolver = match (vault_path(), passphrase) {
        (Some(path), Some(pass)) => match load_vault(&path, &pass) {
            Some(vault) => SecretResolver::with_vault(vault),
            None => SecretResolver::new(),
        },
        _ => SecretResolver::new(),
    };
    Arc::new(resolver)
}

/// The home `~/.dadhichi` directory, if a home is known. The durable, global
/// place user-installed skills and connectors are written so they survive across
/// projects and restarts.
fn home_dadhichi() -> Option<PathBuf> {
    let nonempty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    // DADHICHI_HOME relocates all user-level state (tests/CI isolation).
    std::env::var_os("DADHICHI_HOME")
        .and_then(nonempty)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(nonempty)
        })
        .map(|home| PathBuf::from(home).join(".dadhichi"))
}

/// Reduce a skill name to a safe, lowercase filename stem — alphanumerics kept,
/// every other character folded to `-`. Prevents a crafted `name` from escaping
/// the skills directory or colliding with path separators.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Where an imported skill manifest is written so the loader (and file watcher)
/// pick it up: `~/.dadhichi/skills`, falling back to `<root>/.dadhichi/skills`
/// when no home directory is set.
fn writable_skills_dir(root: &Path) -> PathBuf {
    home_dadhichi()
        .map(|d| d.join("skills"))
        .unwrap_or_else(|| root.join(".dadhichi").join("skills"))
}

/// Where a runtime-added MCP connector is persisted: `~/.dadhichi/mcp.json`,
/// falling back to `<root>/.dadhichi/mcp.json`. Both are standard discovery
/// locations, so a saved connector reconnects on the next launch.
fn writable_mcp_config_path(root: &Path) -> PathBuf {
    home_dadhichi()
        .map(|d| d.join("mcp.json"))
        .unwrap_or_else(|| root.join(".dadhichi").join("mcp.json"))
}

/// The credential-vault path: `$DADHICHI_VAULT`, else `~/.dadhichi/vault.json`.
fn vault_path() -> Option<PathBuf> {
    let nonempty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    if let Some(explicit) = std::env::var_os("DADHICHI_VAULT").and_then(nonempty) {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(nonempty)?;
    Some(PathBuf::from(home).join(".dadhichi").join("vault.json"))
}

/// Load and unlock the vault persisted at `path`. Returns `None` if the file is
/// absent or unparsable; a wrong passphrase is only detected later, on decrypt.
fn load_vault(path: &Path, passphrase: &str) -> Option<Vault> {
    let text = std::fs::read_to_string(path).ok()?;
    let data: VaultData = serde_json::from_str(&text).ok()?;
    Some(Vault::with_data(passphrase, data))
}

impl std::fmt::Debug for AppController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppController").finish_non_exhaustive()
    }
}

impl AppController {
    /// Boot the IDE: register services and commands, populate the palette, and
    /// subscribe the UI to the event bus. `root` is the workspace folder.
    pub async fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let kernel = Kernel::new();

        // Core services, shared with the command handlers. Providers are
        // resolved from the environment (ANTHROPIC_API_KEY / OPENAI_API_KEY /
        // OLLAMA_HOST / …); with none set the offline mock provider is used.
        let plan = ProviderPlan::from_env();
        let model_id = plan.default_model();
        let router = Arc::new(plan.build_router());
        // Tool-approval gate: consequential capabilities (running shell commands,
        // writing the workspace) are set to *interrupt*, so every such call pauses
        // for a human `y/n` routed through the bus. The `BusApprover` parks each
        // call on a one-shot channel that `resolve_approval` fires. Read-only work
        // stays un-gated, so ordinary agent runs aren't interrupted.
        let approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        // The agent's sandboxed virtual filesystem: a workspace-backed state
        // store confined by a PathJail to `root`, so an fs.write can never
        // escape the project. Shared behind the fs.* tools.
        let fs_store: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&root));
        // Shared memory the agent and its delegates record to / recall from,
        // resumed from the workspace's persisted session (the same
        // `.dadhichi/session.json` the CLI and chat REPL use) so the agent
        // remembers previous runs and previous sessions alike.
        let agent_memory = shared_memory();
        let prior_session = session::load(&root);
        if !prior_session.is_empty()
            && let Ok(mut mem) = agent_memory.lock()
        {
            *mem = session::seed_memory(&prior_session);
        }

        let tools = {
            let t = ToolRegistry::new();
            t.register(Arc::new(EchoTool));
            // Shell is pinned to the sandbox root (the run-commands sandbox).
            t.register(Arc::new(TerminalTool::in_dir(&root)));
            // Virtual filesystem tools (context offloading), state backend above.
            t.register(Arc::new(FsReadTool::new(fs_store.clone())));
            t.register(Arc::new(FsWriteTool::new(fs_store.clone())));
            t.register(Arc::new(FsListTool::new(fs_store.clone())));
            // Search tools: content grep and filename glob over the sandbox.
            t.register(Arc::new(FsGrepTool::new(fs_store.clone())));
            t.register(Arc::new(FsGlobTool::new(fs_store.clone())));
            // Full-stack dev tools: scaffold projects, build, test, and query a
            // database — all pinned to the sandbox root and gated on RunCommands.
            t.register(Arc::new(ScaffoldTool::new(&root)));
            t.register(Arc::new(BuildTool::new(&root)));
            t.register(Arc::new(TestRunnerTool::new(&root)));
            t.register(Arc::new(DbQueryTool::new(&root)));
            // Memory-access tools over the shared store.
            t.register(Arc::new(MemoryWriteTool::new(agent_memory.clone())));
            t.register(Arc::new(MemoryRecallTool::new(agent_memory.clone())));
            t.set_policy(
                ApprovalPolicy::default()
                    .with(Permission::RunCommands, PermissionMode::Interrupt)
                    .with(Permission::WriteWorkspace, PermissionMode::Interrupt),
            );
            t.set_approver(Arc::new(BusApprover::new(
                kernel.bus().clone(),
                approvals.clone(),
            )));
            Arc::new(t)
        };
        let store = Arc::new(SqliteSymbolStore::in_memory().expect("open symbol store"));
        let indexer = Indexer::new(store.clone()).with_event_bus(kernel.bus().clone());

        // MCP connectors: launch the servers declared in mcp.json and bridge
        // their tools into the shared registry (the registry is interior-mutable
        // so this reaches the tools every agent already holds). Secrets in the
        // config are `${...}` placeholders resolved from the environment or the
        // encrypted credential vault.
        let (discovered_config, mcp_cfg_errors) = McpServersConfig::discover_in(&root);
        let secrets = mcp_secret_resolver();
        let (mcp_conns, mcp_report) =
            connect_servers(&discovered_config, &tools, |key| secrets.resolve(key)).await;
        let mcp = Arc::new(Mutex::new(mcp_conns));
        let mcp_config = Arc::new(Mutex::new(discovered_config));

        // The skill library: the built-ins plus any user/project skills
        // discovered on disk (~/.dadhichi/skills, <root>/.dadhichi/skills,
        // $DADHICHI_SKILLS_DIR). Held behind a lock so it can be swapped live;
        // `skill.run` builds a fresh agent from it on each dispatch, so reloaded
        // skills take effect immediately.
        let (initial_skills, skill_load) = SkillRegistry::discover_in(&root);
        let skills: SharedSkills = shared(initial_skills);

        // Watch the manifest directories: when a skill file changes on disk, the
        // catalogue is reloaded and a `skill.reloaded` event is published (which
        // `pump` turns into a palette refresh). Failure to start the watch is
        // non-fatal — manual `skill.reload` still works.
        let skill_watch = {
            let bus = kernel.bus().clone();
            let skills_cb = skills.clone();
            watch_skills(skills.clone(), &root, move |report| {
                for err in &report.errors {
                    bus.publish(Event::new(
                        "skill.load.error",
                        serde_json::json!({ "error": err.to_string() }),
                    ));
                }
                let count = skills_cb.read().map(|r| r.len()).unwrap_or(0);
                bus.publish(Event::new(
                    "skill.reloaded",
                    serde_json::json!({ "count": count, "loaded": report.loaded }),
                ));
            })
            .ok()
        };

        // The active model, switchable at runtime (`agent.run` builds its
        // orchestrator from whatever this holds at dispatch time).
        let current_model = Arc::new(std::sync::RwLock::new(model_id.clone()));
        let orchestrator = build_orchestrator(&model_id, &root);

        // Register the `task` delegation tool so any agent (or a model tool-loop)
        // can spawn a specialist with an isolated context. Sub-agents run under a
        // read-only grant, matching the default top-level run.
        tools.register(Arc::new(TaskTool::new(
            orchestrator.clone(),
            router.clone(),
            &tools,
            kernel.bus().clone(),
            GrantSet::from_iter([Permission::ReadWorkspace]),
        )));

        register_commands(
            &kernel,
            &router,
            &tools,
            &current_model,
            &skills,
            &mcp_config,
            &mcp,
            &secrets,
            &model_id,
            &root,
            &indexer,
            &agent_memory,
        )
        .await;

        // Build the UI and seed the palette from the registered command names
        // (default mode), the skill catalogue (the `>` skill mode), and the MCP
        // servers with their live connection state (the `@` mode).
        let mut ui = App::new();
        ui.open_workspace(&root);
        ui.set_commands(kernel.commands().command_names().await);
        ui.palette
            .set_skills(skill_entries(&skills.read().expect("skills lock")));
        ui.palette.set_mcp_servers(mcp_entries(
            &mcp_config.lock().expect("mcp config lock"),
            &mcp.lock().expect("mcp lock"),
        ));

        let events = kernel.bus().subscribe();

        // Surface any malformed skill manifests on the bus now that a
        // subscriber exists, so they show up in the console rather than failing
        // silently. Successful loads are reflected in the palette/skill list.
        for err in &skill_load.errors {
            kernel.bus().publish(Event::new(
                "skill.load.error",
                serde_json::json!({ "error": err.to_string() }),
            ));
        }

        // Report the boot-time MCP connection results to the console.
        for err in &mcp_cfg_errors {
            kernel
                .bus()
                .publish(Event::new("mcp.error", serde_json::json!({ "error": err })));
        }
        publish_mcp_report(kernel.bus(), &mcp_report);

        // Language servers, spawned lazily per language on the first completion
        // request. Their diagnostics feed the same bus the Problems panel reads.
        let lsp = Arc::new(LspManager::new(&root, Some(kernel.bus().clone())));

        Self {
            kernel,
            ui,
            events,
            store,
            skills,
            _skill_watch: skill_watch,
            mcp_config,
            mcp,
            tools,
            approvals,
            router: router.clone(),
            root: root.clone(),
            model_id: model_id.clone(),
            current_model,
            pending_delegation: Arc::new(Mutex::new(None)),
            lsp,
        }
    }

    /// The permission-gated tool registry (built-in tools plus any bridged in
    /// from connected MCP servers). Invocations pass through the grant check and
    /// the approval policy.
    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// A fresh subscription to every kernel event — how alternative frontends
    /// (the web GUI) stream agent progress, diagnostics, terminal output, and
    /// approval prompts without going through the TUI view-models.
    pub fn subscribe_events(&self) -> Subscription {
        self.kernel.bus().subscribe()
    }

    /// The workspace root this controller serves.
    pub fn workspace_root(&self) -> &Path {
        &self.root
    }

    /// The model id runs currently execute under (for display).
    pub fn model_label(&self) -> String {
        self.current_model
            .read()
            .map(|m| m.clone())
            .unwrap_or_else(|_| self.model_id.clone())
    }

    /// Switch the model future `agent.run`s execute under. The provider stays
    /// as resolved at boot (the router routes unknown model names to the
    /// default provider), so for Ollama this changes which local/cloud model
    /// the requests name. Announced as a `model.changed` event.
    pub fn set_model(&self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        if let Ok(mut current) = self.current_model.write() {
            *current = model.to_string();
        }
        self.kernel.bus().publish(Event::new(
            "model.changed",
            serde_json::json!({ "model": model }),
        ));
    }

    /// Immutable access to the UI view-models.
    pub fn ui(&self) -> &App {
        &self.ui
    }

    /// Mutable access to the UI view-models (for the frontend's input handling).
    pub fn ui_mut(&mut self) -> &mut App {
        &mut self.ui
    }

    /// Write the active editor buffer back to its file on disk, clearing its
    /// dirty flag and reflecting the result in the status bar. Returns `Ok(false)`
    /// when there is nothing to save (no active buffer, or a scratch buffer with
    /// no path). This is the real work behind the editor's Ctrl-S.
    pub fn save_active_document(&mut self) -> std::io::Result<bool> {
        let Some(doc) = self.ui.active_document() else {
            return Ok(false);
        };
        let Some(path) = doc.path.clone() else {
            self.ui.status = "cannot save: buffer has no path".into();
            return Ok(false);
        };
        let text = doc.text();
        std::fs::write(&path, text)?;
        if let Some(doc) = self.ui.active_document_mut() {
            doc.mark_saved();
        }
        self.ui.status = format!("saved {}", path.display());
        self.kernel.bus().publish(Event::new(
            "editor.saved",
            serde_json::json!({ "ok": true, "path": path.display().to_string() }),
        ));
        // Refresh the file's diagnostics now that its saved text changed.
        self.sync_active_document();
        Ok(true)
    }

    /// The symbol store, for code-intelligence queries.
    pub fn store(&self) -> &Arc<SqliteSymbolStore> {
        &self.store
    }

    /// The shared skill catalogue — for a frontend to list equippable skills.
    /// It sits behind a lock because `skill.reload` can swap it live; read it
    /// with `ctrl.skills().read().unwrap()`. Skills run via `skill.run`/`skill.list`.
    pub fn skills(&self) -> &SharedSkills {
        &self.skills
    }

    /// Dispatch a command by name (with optional JSON args) through the kernel.
    pub async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        self.kernel
            .commands()
            .dispatch(Command {
                name: name.into(),
                args,
            })
            .await
    }

    /// Start the conversational agent against a free-text `goal` typed into the
    /// agent console, **without blocking**. The dispatch runs on a spawned task,
    /// so the render loop keeps pumping and the run's `agent.*` events stream into
    /// the console as they arrive — essential when a local model takes seconds to
    /// respond. The command registry is a cheap `Arc` handle, so the clone is
    /// free; a `mock.error` is published if the agent errors so the console shows
    /// it rather than swallowing it.
    pub fn start_agent_goal(&self, goal: &str) {
        let commands = self.kernel.commands().clone();
        let bus = self.kernel.bus().clone();
        let goal = goal.to_string();
        tokio::spawn(async move {
            let result = commands
                .dispatch(Command {
                    name: "agent.run".into(),
                    args: serde_json::json!({ "goal": goal }),
                })
                .await;
            if let Err(err) = result {
                bus.publish(Event::new(
                    "agent.error",
                    serde_json::json!({ "error": err.to_string() }),
                ));
            }
        });
    }

    /// Request LSP completions for the active buffer at its cursor, **without
    /// blocking**. The manager spawns/reuses the language server for the file's
    /// language, syncs the buffer's *current* text (unsaved edits included),
    /// and requests suggestions on a spawned task; the result comes back
    /// through the bus as an `lsp.completion` event (or `lsp.status` on
    /// failure), which `pump` routes into the completion popup. This is the
    /// real work behind the editor's Ctrl+Space.
    pub fn request_completions(&mut self) {
        let Some(doc) = self.ui.active_document() else {
            return;
        };
        let Some(path) = doc.path.clone() else {
            self.ui.status = "no language server for a scratch buffer".into();
            return;
        };
        let text = doc.text();
        let (line, col) = doc.cursor_line_col();
        self.ui.status = "completing…".into();
        self.request_completions_at(path, text, line as u32, col as u32);
    }

    /// Request LSP completions for any `path`/`text`/cursor, **without
    /// blocking** — the path-based seam alternative frontends (the web GUI)
    /// drive directly. The result is published as an `lsp.completion` event
    /// (or a quiet `lsp.status` on failure).
    pub fn request_completions_at(&self, path: PathBuf, text: String, line: u32, col: u32) {
        let position = dadhichi_lsp::Position::new(line, col);
        let lsp = self.lsp.clone();
        let bus = self.kernel.bus().clone();
        tokio::spawn(async move {
            match lsp.completions(&path, &text, position).await {
                Ok(items) => {
                    let payload = serde_json::json!({
                        "path": path.display().to_string(),
                        "items": serde_json::to_value(&items).unwrap_or_default(),
                    });
                    bus.publish(Event::new("lsp.completion", payload));
                }
                Err(err) => {
                    bus.publish(Event::new(
                        "lsp.status",
                        serde_json::json!({ "message": err.to_string() }),
                    ));
                }
            }
        });
    }

    /// Push the active buffer's current text to its language server **without
    /// blocking**, so the server publishes fresh diagnostics for it (routed to
    /// the Problems panel via `lsp.diagnostics`). Servers only report problems
    /// for documents they've been told about, so this runs on file open and
    /// save — not just on completion requests. Failures (usually: the language
    /// server isn't installed) surface as a quiet `lsp.status` event.
    pub fn sync_active_document(&self) {
        let Some(doc) = self.ui.active_document() else {
            return;
        };
        let Some(path) = doc.path.clone() else {
            return;
        };
        let text = doc.text();
        self.sync_document_at(path, text);
    }

    /// Push any `path`/`text` to its language server so diagnostics refresh —
    /// the path-based seam alternative frontends (the web GUI) drive directly.
    pub fn sync_document_at(&self, path: PathBuf, text: String) {
        // HTML gets a built-in structural check: the HTML language server
        // reports nothing for unclosed/crossed tags, so those are found here
        // and published straight onto the Problems seam.
        let is_html = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
        if is_html {
            let problems: Vec<serde_json::Value> = htmlcheck::diagnostics(&text)
                .into_iter()
                .map(|(line, message)| {
                    serde_json::json!({
                        "range": { "start": { "line": line, "character": 0 },
                                   "end": { "line": line, "character": 1 } },
                        "severity": "error",
                        "message": message,
                    })
                })
                .collect();
            self.kernel.bus().publish(Event::new(
                "lsp.diagnostics",
                serde_json::json!({
                    "uri": path.display().to_string(),
                    "diagnostics": problems,
                }),
            ));
        }

        let lsp = self.lsp.clone();
        let bus = self.kernel.bus().clone();
        tokio::spawn(async move {
            if let Err(err) = lsp.sync(&path, &text).await {
                bus.publish(Event::new(
                    "lsp.status",
                    serde_json::json!({ "message": err.to_string() }),
                ));
            }
        });
    }

    /// Run a shell command through the gated `terminal.run` tool **without
    /// blocking**. Because `terminal.run` requires `RunCommands` — set to
    /// *interrupt* — the call pauses and publishes an `agent.approval` event; the
    /// render loop keeps pumping (the dispatch is spawned), so the TUI can show
    /// the prompt and the user's `y/n` unblocks it via [`resolve_approval`]. The
    /// command's `terminal.result` / `terminal.error` events stream the outcome
    /// into the console.
    ///
    /// [`resolve_approval`]: Self::resolve_approval
    pub fn start_terminal(&self, command: &str) {
        let commands = self.kernel.commands().clone();
        let bus = self.kernel.bus().clone();
        let command = command.to_string();
        tokio::spawn(async move {
            let result = commands
                .dispatch(Command {
                    name: "terminal.run".into(),
                    args: serde_json::json!({ "command": command }),
                })
                .await;
            if let Err(err) = result {
                bus.publish(Event::new(
                    "terminal.error",
                    serde_json::json!({ "error": err.to_string() }),
                ));
            }
        });
    }

    /// Answer a pending tool-approval prompt: fire the parked one-shot so the
    /// suspended tool call proceeds (or is rejected), and announce the verdict on
    /// the bus as `agent.approval.resolved` (which clears the UI prompt). A no-op
    /// if `id` isn't a live request.
    pub fn resolve_approval(&self, id: &str, decision: Decision) {
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            return;
        };
        let sender = self
            .approvals
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&uuid));
        if let Some(sender) = sender {
            let _ = sender.send(decision);
            let verdict = match decision {
                Decision::Approve => "approve",
                Decision::Deny => "deny",
            };
            self.kernel.bus().publish(Event::new(
                "agent.approval.resolved",
                serde_json::json!({ "id": id, "decision": verdict }),
            ));
        }
    }

    /// Delegate `task` to the named `subagent` specialist **without blocking**.
    /// The delegate runs in a copy-on-write overlay so its file writes are staged;
    /// an orchestrator-side critic reviews the result; work that clears the
    /// confidence threshold lands and commits to the branch automatically, while
    /// work below it raises the Review panel (`agent.delegation.review`) for a
    /// land/discard decision routed through [`resolve_delegation`].
    ///
    /// [`resolve_delegation`]: Self::resolve_delegation
    pub fn start_delegation(&self, subagent: &str, task: &str) {
        let Some(spec) = SubAgentSpec::for_role(subagent) else {
            self.kernel.bus().publish(Event::new(
                "agent.error",
                serde_json::json!({
                    "error": format!(
                        "unknown specialist '{subagent}'; try one of: {}",
                        SubAgentSpec::roster().join(", ")
                    )
                }),
            ));
            return;
        };
        // Fold the spec's skills into the delegate's persona from the live library.
        let persona = self.equip_delegate_persona(&spec);

        let router = self.router.clone();
        let bus = self.kernel.bus().clone();
        let root = self.root.clone();
        let model_id = self.model_id.clone();
        let task = task.to_string();
        let pending = self.pending_delegation.clone();
        let spec_name = spec.name.clone();

        tokio::spawn(async move {
            let base: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&root));
            let critic = Arc::new(ModelCritic::new(router.clone(), &model_id));
            let delegator = Delegator::new(router.clone(), bus.clone()).with_critic(critic);
            let agent = ReactAgent::new(&model_id).as_role(&spec_name, persona);

            let review = match delegator.delegate(&agent, &spec, &task, base, &root).await {
                Ok(review) => review,
                Err(err) => {
                    bus.publish(Event::new(
                        "agent.error",
                        serde_json::json!({ "error": err.to_string() }),
                    ));
                    return;
                }
            };

            if review.auto_approved() {
                // Verified above the threshold — land and commit automatically.
                land_and_commit(&review, &root, &spec_name, &task, &bus);
            } else if review.has_changes() {
                // Below the threshold — hold the work for a human decision.
                bus.publish(Event::new(
                    "agent.delegation.review",
                    delegation_review_payload(&spec_name, &review),
                ));
                *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingDelegation {
                    subagent: spec_name,
                    task,
                    review,
                });
            } else {
                // A read-only specialist (reviewer, auditor) — nothing to land.
                bus.publish(Event::new(
                    "agent.delegation.resolved",
                    serde_json::json!({ "outcome": "reported (no changes to land)" }),
                ));
            }
        });
    }

    /// Answer the pending delegation review: on approval, land the staged work
    /// (flush the overlay onto the workspace and commit it to the branch); on
    /// rejection, discard it. Either way clears the Review panel via
    /// `agent.delegation.resolved`. A no-op if no review is pending.
    pub fn resolve_delegation(&self, approve: bool) {
        let pending = self
            .pending_delegation
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let Some(pending) = pending else {
            return;
        };
        let bus = self.kernel.bus();
        if approve {
            land_and_commit(
                &pending.review,
                &self.root,
                &pending.subagent,
                &pending.task,
                bus,
            );
        } else {
            bus.publish(Event::new(
                "agent.delegation.resolved",
                serde_json::json!({ "outcome": "discarded — nothing committed" }),
            ));
        }
    }

    /// Fold a spec's equipped skills' instructions into its persona, resolving
    /// their names against the live skill library.
    fn equip_delegate_persona(&self, spec: &SubAgentSpec) -> String {
        let mut persona = spec.persona.clone();
        if let Ok(registry) = self.skills.read() {
            for name in &spec.skills {
                if let Some(skill) = registry.get(name) {
                    persona.push_str(&format!(
                        "\n\nSkill — {}: {}",
                        skill.name, skill.instructions
                    ));
                }
            }
        }
        persona
    }

    /// Whether a delegation review is on screen awaiting the user's decision.
    pub fn has_pending_delegation(&self) -> bool {
        self.pending_delegation
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    /// Accept the highlighted palette entry, close the palette, and dispatch it:
    /// a command runs directly; a skill (from `>` skill mode) runs via
    /// `skill.run`. Returns a label for what was dispatched, if anything.
    pub async fn run_palette_selection(&mut self) -> Option<String> {
        let action = self.ui.palette.accept()?;
        self.ui.palette.close();
        let label = match action {
            PaletteAction::RunCommand(name) => {
                let _ = self.dispatch(&name, serde_json::json!({})).await;
                name
            }
            PaletteAction::RunSkill(name) => {
                let _ = self
                    .dispatch("skill.run", serde_json::json!({ "skill": name }))
                    .await;
                format!("skill.run:{name}")
            }
            PaletteAction::ConnectMcp(name) => {
                let _ = self
                    .dispatch("mcp.connect", serde_json::json!({ "server": name }))
                    .await;
                format!("mcp.connect:{name}")
            }
            PaletteAction::DisconnectMcp(name) => {
                let _ = self
                    .dispatch("mcp.disconnect", serde_json::json!({ "server": name }))
                    .await;
                format!("mcp.disconnect:{name}")
            }
            PaletteAction::AddMcp(connector) => {
                let _ = self
                    .dispatch("mcp.add", serde_json::json!({ "connector": connector }))
                    .await;
                format!("mcp.add:{connector}")
            }
        };
        // A command may have changed the skill set (e.g. `skill.reload`) or the
        // connection state; keep both palette lists current.
        self.refresh_skills();
        self.refresh_mcp();
        Some(label)
    }

    /// Refresh the palette's skill list from the current catalogue. Call after
    /// anything that may have changed the skills (a `skill.reload`, or the file
    /// watcher firing).
    pub fn refresh_skills(&mut self) {
        if let Ok(registry) = self.skills.read() {
            let entries = skill_entries(&registry);
            self.ui.palette.set_skills(entries);
        }
    }

    /// Refresh the palette's MCP-server list (names, transport, tool count, and
    /// connected state). Call after a connect/disconnect.
    pub fn refresh_mcp(&mut self) {
        if let (Ok(config), Ok(connections)) = (self.mcp_config.lock(), self.mcp.lock()) {
            let entries = mcp_entries(&config, &connections);
            self.ui.palette.set_mcp_servers(entries);
        }
    }

    /// Drain all currently-buffered bus events into the UI view-models. Returns
    /// how many were applied. Call once per frame — it never blocks.
    pub fn pump(&mut self) -> usize {
        let mut applied = 0;
        let mut skills_changed = false;
        let mut mcp_changed = false;
        let mut tree_changed = false;
        loop {
            match self.events.try_recv() {
                Ok(Some(event)) => {
                    match event.topic.as_str() {
                        // The file watcher reloads the catalogue off-thread and
                        // announces it here; refresh the palette's skill list so
                        // the `>` picker reflects the change without a keystroke.
                        "skill.reloaded" => skills_changed = true,
                        // A connect/disconnect (including from another surface)
                        // or a newly-added connector changed the `@` server list.
                        "mcp.connected" | "mcp.disconnected" | "mcp.error" | "mcp.added" => {
                            mcp_changed = true
                        }
                        // A save, or an agent run that may have written files,
                        // can change what is on disk — re-scan the Explorer so
                        // new files appear without reopening the workspace.
                        "editor.saved" => tree_changed = true,
                        "agent.status" => {
                            if event.payload.get("status").and_then(|s| s.as_str())
                                == Some("completed")
                            {
                                tree_changed = true;
                            }
                        }
                        _ => {}
                    }
                    self.ui.apply_event(&event);
                    applied += 1;
                }
                Ok(None) => break,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        if skills_changed {
            self.refresh_skills();
        }
        if mcp_changed {
            self.refresh_mcp();
        }
        if tree_changed && let Some(explorer) = self.ui.explorer.as_mut() {
            explorer.refresh();
        }
        applied
    }
}

/// Register the IDE's commands. Each runs real work and emits bus events, so the
/// UI updates purely by pumping the bus.
/// The `agent.delegation.review` payload: the verdict plus the staged change set
/// the Review panel renders.
fn delegation_review_payload(subagent: &str, review: &DelegationReview) -> serde_json::Value {
    serde_json::json!({
        "subagent": subagent,
        "verdict": review.verdict_note(),
        "files": review
            .changes
            .iter()
            .map(|c| serde_json::json!({ "path": c.path, "deleted": c.deleted }))
            .collect::<Vec<_>>(),
    })
}

/// Land a reviewed delegation: flush its overlay onto the workspace, then commit
/// the result to the current branch, announcing the outcome on the bus.
fn land_and_commit(
    review: &DelegationReview,
    root: &Path,
    subagent: &str,
    task: &str,
    bus: &dadhichi_core::EventBus,
) {
    let landed = match review.land() {
        Ok(n) => n,
        Err(err) => {
            bus.publish(Event::new(
                "agent.delegation.resolved",
                serde_json::json!({ "outcome": format!("failed to land: {err}") }),
            ));
            return;
        }
    };
    match commit_delegation(root, subagent, task) {
        Ok(id) => {
            bus.publish(Event::new(
                "agent.delegation.landed",
                serde_json::json!({ "commit": id, "files": landed, "subagent": subagent }),
            ));
        }
        Err(err) => {
            bus.publish(Event::new(
                "agent.delegation.resolved",
                serde_json::json!({
                    "outcome": format!("landed {landed} file(s) but commit failed: {err}")
                }),
            ));
        }
    }
}

/// Stage and commit the landed changes to the repo at `root`, attributed to the
/// sub-agent (author from `GIT_AUTHOR_*`, falling back to a sub-agent identity).
fn commit_delegation(root: &Path, subagent: &str, task: &str) -> Result<String, String> {
    let repo = GitRepo::open(root).map_err(|e| e.to_string())?;
    repo.stage_all().map_err(|e| e.to_string())?;
    let name = std::env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "dadhichi-agent".to_string());
    let email =
        std::env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "agent@dadhichi.local".to_string());
    repo.commit(&format!("{subagent}: {task}"), &name, &email)
        .map_err(|e| e.to_string())
}

/// Build the palette's skill rows (name, description, capability summary) from
/// the catalogue.
fn skill_entries(registry: &SkillRegistry) -> Vec<SkillEntry> {
    registry
        .list()
        .into_iter()
        .map(|spec| SkillEntry {
            detail: skill_detail(&spec),
            name: spec.name,
            description: spec.description,
        })
        .collect()
}

/// A one-line, secret-free capability summary for a skill.
fn skill_detail(spec: &SkillSpec) -> String {
    let perms = if spec.permissions.is_empty() {
        "none".to_string()
    } else {
        spec.permissions
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let tools = match &spec.tools {
        SkillTools::None => "none".to_string(),
        SkillTools::Any => "any".to_string(),
        SkillTools::Allow(_) => spec.tools.names().join(", "),
    };
    format!("perms: {perms} · tools: {tools}")
}

/// Publish `mcp.connected` for each connected server and `mcp.error` for each
/// failure, so a connect pass shows up in the console.
fn publish_mcp_report(bus: &dadhichi_core::EventBus, report: &dadhichi_mcp::ConnectReport) {
    for server in &report.connected {
        bus.publish(Event::new(
            "mcp.connected",
            serde_json::json!({ "server": server.name, "tools": server.tools }),
        ));
    }
    for err in &report.errors {
        bus.publish(Event::new(
            "mcp.error",
            serde_json::json!({ "server": err.server, "error": err.message }),
        ));
    }
}

/// Build the palette's MCP-server rows: every configured server with its
/// transport, tool count, and connected state, followed by the built-in
/// connectors not yet configured (shown as "add" actions), so the `@` palette is
/// both a control panel and a catalogue.
fn mcp_entries(config: &McpServersConfig, connections: &McpConnections) -> Vec<McpEntry> {
    let mut entries: Vec<McpEntry> = config
        .servers
        .iter()
        .map(|(name, cfg)| {
            let connected = connections.contains(name);
            let detail = if connected {
                let n = connections.tools_of(name).len();
                let plural = if n == 1 { "" } else { "s" };
                format!("{} · {n} tool{plural} · connected", cfg.transport())
            } else if !cfg.enabled {
                format!("{} · disabled", cfg.transport())
            } else {
                format!("{} · offline", cfg.transport())
            };
            McpEntry {
                name: name.clone(),
                detail,
                connected,
                available: false,
            }
        })
        .collect();

    // Append catalogue connectors the user hasn't added yet.
    for c in dadhichi_mcp::builtin_connectors() {
        if config.servers.contains_key(c.id) {
            continue;
        }
        let secret = if c.needs_secrets() {
            " · needs secret"
        } else {
            ""
        };
        entries.push(McpEntry {
            name: c.id.to_string(),
            detail: format!("add · {}{secret}", c.description),
            connected: false,
            available: true,
        });
    }
    entries
}

/// Connect the servers in `config` (optionally just `only`), merging the results
/// into the shared `mcp` set and publishing a report on `bus`. Returns the JSON
/// summary the `mcp.connect` command replies with.
async fn connect_and_merge(
    config: &McpServersConfig,
    only: Option<&str>,
    tools: &Arc<ToolRegistry>,
    mcp: &Arc<Mutex<McpConnections>>,
    secrets: &Arc<SecretResolver>,
    bus: &dadhichi_core::EventBus,
) -> serde_json::Value {
    // For a targeted connect, narrow to that one server and force it enabled so
    // an explicit request overrides a `"enabled": false` in the config.
    let scoped = match only {
        Some(name) => {
            let mut one = McpServersConfig::default();
            if let Some(cfg) = config.servers.get(name) {
                let mut cfg = cfg.clone();
                cfg.enabled = true;
                one.servers.insert(name.to_string(), cfg);
            }
            one
        }
        None => config.clone(),
    };

    let (fresh, report) = connect_servers(&scoped, tools, |key| secrets.resolve(key)).await;
    publish_mcp_report(bus, &report);
    let connected = fresh.names();
    if let Ok(mut guard) = mcp.lock() {
        guard.merge(fresh);
    }
    serde_json::json!({
        "connected": connected,
        "tools": report.tool_count(),
        "errors": report.errors.len(),
    })
}

/// A snapshot of the currently-connected servers as `(name, connection)` pairs.
/// Taken under the lock and returned owned, so the RPCs that follow never hold
/// the mutex across an `.await`.
fn snapshot_connections(
    mcp: &Arc<Mutex<McpConnections>>,
) -> Result<Vec<(String, Arc<McpConnection>)>, KernelError> {
    let guard = mcp
        .lock()
        .map_err(|_| KernelError::command_failed("mcp connections lock poisoned"))?;
    Ok(guard
        .names()
        .into_iter()
        .filter_map(|name| guard.connection(&name).map(|conn| (name, conn)))
        .collect())
}

/// The live connection for `server`, or a command error if it isn't connected.
fn connection_of(
    mcp: &Arc<Mutex<McpConnections>>,
    server: &str,
) -> Result<Arc<McpConnection>, KernelError> {
    mcp.lock()
        .map_err(|_| KernelError::command_failed("mcp connections lock poisoned"))?
        .connection(server)
        .ok_or_else(|| KernelError::command_failed(format!("server not connected: {server}")))
}

/// Register `mcp.resources`, `mcp.resource.read`, `mcp.prompts`, and
/// `mcp.prompt.get`, which surface the readable resources and prompt templates
/// the connected servers expose.
async fn register_mcp_capability_commands(kernel: &Kernel, mcp: &Arc<Mutex<McpConnections>>) {
    // mcp.resources — list every connected server's resources, namespaced.
    {
        let mcp = mcp.clone();
        kernel
            .commands()
            .register(
                "mcp.resources",
                Arc::new(move |_cmd: Command| {
                    let mcp = mcp.clone();
                    async move {
                        let servers = snapshot_connections(&mcp)?;
                        let mut resources = Vec::new();
                        for (server, conn) in servers {
                            if let Ok(list) = conn.list_resources().await {
                                for r in list {
                                    resources.push(serde_json::json!({
                                        "server": server,
                                        "uri": r.uri,
                                        "name": r.name,
                                        "description": r.description,
                                        "mimeType": r.mime_type,
                                    }));
                                }
                            }
                        }
                        Ok(serde_json::json!({ "resources": resources }))
                    }
                }),
            )
            .await;
    }

    // mcp.resource.read — read one resource: { server, uri }.
    {
        let mcp = mcp.clone();
        kernel
            .commands()
            .register(
                "mcp.resource.read",
                Arc::new(move |cmd: Command| {
                    let mcp = mcp.clone();
                    async move {
                        let server = str_arg(&cmd, "server")?;
                        let uri = str_arg(&cmd, "uri")?;
                        let conn = connection_of(&mcp, &server)?;
                        let contents = conn
                            .read_resource(&uri)
                            .await
                            .map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({ "server": server, "uri": uri, "contents": contents }))
                    }
                }),
            )
            .await;
    }

    // mcp.prompts — list every connected server's prompt templates, namespaced.
    {
        let mcp = mcp.clone();
        kernel
            .commands()
            .register(
                "mcp.prompts",
                Arc::new(move |_cmd: Command| {
                    let mcp = mcp.clone();
                    async move {
                        let servers = snapshot_connections(&mcp)?;
                        let mut prompts = Vec::new();
                        for (server, conn) in servers {
                            if let Ok(list) = conn.list_prompts().await {
                                for p in list {
                                    prompts.push(serde_json::json!({
                                        "server": server,
                                        "name": p.name,
                                        "description": p.description,
                                        "arguments": p.arguments.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                                    }));
                                }
                            }
                        }
                        Ok(serde_json::json!({ "prompts": prompts }))
                    }
                }),
            )
            .await;
    }

    // mcp.prompt.get — instantiate a prompt: { server, name, args? }.
    {
        let mcp = mcp.clone();
        kernel
            .commands()
            .register(
                "mcp.prompt.get",
                Arc::new(move |cmd: Command| {
                    let mcp = mcp.clone();
                    async move {
                        let server = str_arg(&cmd, "server")?;
                        let name = str_arg(&cmd, "name")?;
                        let args = cmd
                            .args
                            .get("args")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({}));
                        let conn = connection_of(&mcp, &server)?;
                        let result = conn
                            .get_prompt(&name, args)
                            .await
                            .map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({ "server": server, "name": name, "prompt": result }))
                    }
                }),
            )
            .await;
    }
}

/// Extract a required string argument, or a command error naming it.
fn str_arg(cmd: &Command, key: &str) -> Result<String, KernelError> {
    cmd.args
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| KernelError::command_failed(format!("missing `{key}` argument")))
}

/// The agent roster for one model: the default ReAct engine, the plain
/// conversational agent, every specialist, and the Claude Code backend.
/// Rebuilt cheaply whenever the active model changes, so a GUI model switch
/// takes effect on the next run.
fn build_orchestrator(model_id: &str, root: &Path) -> Arc<Orchestrator> {
    let mut orch = Orchestrator::new();
    // The tool-using ReAct agent is the default: it can actually perform
    // tasks (run commands, read/write files) via the approval-gated tool
    // loop, not just answer in prose.
    orch.register(Arc::new(ReactAgent::new(model_id)));
    orch.register(Arc::new(ConversationalAgent::new(model_id)));
    // Claude Code as a switchable backend: selected via the model picker
    // (model id "claude-code") or addressed directly as an agent.
    orch.register(Arc::new(ClaudeCodeAgent::new(root)));
    for agent in [
        SpecialistAgent::code(),
        SpecialistAgent::refactor(),
        SpecialistAgent::test(),
        SpecialistAgent::docs(),
        SpecialistAgent::review(),
        SpecialistAgent::git(),
        SpecialistAgent::security(),
        // Full-stack roles for frontend, backend, and database work.
        SpecialistAgent::frontend(),
        SpecialistAgent::backend(),
        SpecialistAgent::database(),
    ] {
        orch.register(Arc::new(agent.with_model(model_id)));
    }
    Arc::new(orch)
}

#[allow(clippy::too_many_arguments)]
async fn register_commands(
    kernel: &Kernel,
    router: &Arc<ModelRouter>,
    tools: &Arc<ToolRegistry>,
    current_model: &Arc<std::sync::RwLock<String>>,
    skills: &SharedSkills,
    mcp_config: &Arc<Mutex<McpServersConfig>>,
    mcp: &Arc<Mutex<McpConnections>>,
    secrets: &Arc<SecretResolver>,
    model_id: &str,
    root: &Path,
    indexer: &Indexer,
    agent_memory: &SharedMemory,
) {
    // agent.run — run an agent against a goal; its progress streams as agent.*.
    {
        let router = router.clone();
        let tools = tools.clone();
        let current_model = current_model.clone();
        let fallback_model = model_id.to_string();
        let bus = kernel.bus().clone();
        let agent_memory = agent_memory.clone();
        let root = root.to_path_buf();
        kernel
            .commands()
            .register(
                "agent.run",
                Arc::new(move |cmd: Command| {
                    let router = router.clone();
                    let tools = tools.clone();
                    let current_model = current_model.clone();
                    let fallback_model = fallback_model.clone();
                    let bus = bus.clone();
                    let agent_memory = agent_memory.clone();
                    let root = root.clone();
                    async move {
                        // The roster is built from the *current* model, so a
                        // switch in the GUI applies to this very run. Selecting
                        // "claude-code" in the picker routes the goal to the
                        // Claude Code backend instead of the native loop.
                        let model = current_model
                            .read()
                            .map(|m| m.clone())
                            .unwrap_or(fallback_model);
                        let default_agent = if model == ClaudeCodeAgent::NAME {
                            ClaudeCodeAgent::NAME
                        } else {
                            "react-agent"
                        };
                        let agent = cmd
                            .args
                            .get("agent")
                            .and_then(|a| a.as_str())
                            .unwrap_or(default_agent)
                            .to_string();
                        let orchestrator = build_orchestrator(&model, &root);
                        let goal = cmd
                            .args
                            .get("goal")
                            .and_then(|g| g.as_str())
                            .unwrap_or("Explain what makes Dadhichi agent-native.")
                            .to_string();

                        // The default run may act, not just read: grant the write
                        // and run-command capabilities too. Each such call is still
                        // stopped at the y/n approval gate before it executes, so a
                        // broad grant here does not mean unattended side effects.
                        let mut ctx = AgentContext::new(
                            router,
                            tools,
                            GrantSet::from_iter([
                                Permission::ReadWorkspace,
                                Permission::WriteWorkspace,
                                Permission::RunCommands,
                            ]),
                            bus,
                        );
                        // Seed the run with everything remembered so far — prior
                        // turns this session, memory.write notes, and the persisted
                        // session from previous boots — so the agent can actually
                        // recall earlier chats instead of starting blank each goal.
                        let seeded = match agent_memory.lock() {
                            Ok(mem) => {
                                ctx.memory.restore(mem.snapshot());
                                ctx.memory.len()
                            }
                            Err(_) => 0,
                        };
                        let result = orchestrator.run(&agent, &goal, &mut ctx).await;
                        // Whatever the outcome, carry this run's new memories (the
                        // goal, tool results, the closing summary) back into the
                        // shared store and persist it, so the next goal — and the
                        // next session — can pick up the thread.
                        if let Ok(mut mem) = agent_memory.lock() {
                            for item in ctx.memory.snapshot().into_iter().skip(seeded) {
                                mem.remember(item.tier, item.content);
                            }
                            let _ = session::save(&root, &mem);
                        }
                        let outcome = result.map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({
                            "agent": agent,
                            "status": format!("{:?}", outcome.status),
                            "confidence": outcome.confidence,
                        }))
                    }
                }),
            )
            .await;
    }

    // agent.spawn — delegate a goal to a specialist that runs in an isolated
    // context, returning only its summary. Args: `{ agent, goal }`. This drives
    // the registered `task` tool, so the delegation streams the same
    // agent.delegated / agent.* events to the console.
    {
        let tools = tools.clone();
        kernel
            .commands()
            .register(
                "agent.spawn",
                Arc::new(move |cmd: Command| {
                    let tools = tools.clone();
                    async move {
                        let agent = cmd
                            .args
                            .get("agent")
                            .and_then(|a| a.as_str())
                            .unwrap_or("code-agent")
                            .to_string();
                        let goal = cmd
                            .args
                            .get("goal")
                            .and_then(|g| g.as_str())
                            .unwrap_or("Describe what you would do.")
                            .to_string();
                        tools
                            .invoke(
                                TaskTool::NAME,
                                serde_json::json!({ "subagent_type": agent, "description": goal }),
                                &GrantSet::none(),
                            )
                            .await
                            .map_err(KernelError::command_failed)
                    }
                }),
            )
            .await;
    }

    // skill.run — equip a skill by name and run it with exactly the permissions
    // it declares, so its `skill.*` progress streams to the console. The skill
    // is fetched from the (reloadable) registry and a fresh agent is built per
    // dispatch, so `skill.reload` takes effect on the very next run.
    {
        let router = router.clone();
        let tools = tools.clone();
        let skills = skills.clone();
        let model_id = model_id.to_string();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "skill.run",
                Arc::new(move |cmd: Command| {
                    let router = router.clone();
                    let tools = tools.clone();
                    let skills = skills.clone();
                    let model_id = model_id.clone();
                    let bus = bus.clone();
                    async move {
                        let skill_name = cmd
                            .args
                            .get("skill")
                            .and_then(|s| s.as_str())
                            .unwrap_or("explain")
                            .to_string();
                        let goal = cmd
                            .args
                            .get("goal")
                            .and_then(|g| g.as_str())
                            .unwrap_or("Explain what a skill is.")
                            .to_string();

                        // Snapshot the skill, then drop the lock before running.
                        let skill = skills
                            .read()
                            .ok()
                            .and_then(|registry| registry.get(&skill_name))
                            .ok_or_else(|| {
                                KernelError::command_failed(format!("unknown skill: {skill_name}"))
                            })?;

                        let agent = SkillAgent::new(skill.clone()).with_model(&model_id);
                        // Grant exactly what the skill requires — no more.
                        let mut ctx =
                            AgentContext::new(router, tools, skill.required_grants(), bus);
                        let outcome = agent
                            .run(&goal, &mut ctx)
                            .await
                            .map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({
                            "skill": skill_name,
                            "status": format!("{:?}", outcome.status),
                            "confidence": outcome.confidence,
                        }))
                    }
                }),
            )
            .await;
    }

    // skill.list — enumerate the available skills as specs (name, description,
    // required permissions, tool scope, step count) for a palette or picker.
    {
        let skills = skills.clone();
        kernel
            .commands()
            .register(
                "skill.list",
                Arc::new(move |_cmd: Command| {
                    let skills = skills.clone();
                    async move {
                        let registry = skills
                            .read()
                            .map_err(|_| KernelError::command_failed("skills lock poisoned"))?;
                        let specs = serde_json::to_value(registry.list())
                            .map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({
                            "count": registry.len(),
                            "skills": specs,
                        }))
                    }
                }),
            )
            .await;
    }

    // skill.reload — re-scan the manifest directories and swap the catalogue
    // live, without restarting. Malformed manifests are reported (not fatal) as
    // `skill.load.error`, and a `skill.reloaded` event announces the new count.
    {
        let skills = skills.clone();
        let root = root.to_path_buf();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "skill.reload",
                Arc::new(move |_cmd: Command| {
                    let skills = skills.clone();
                    let root = root.clone();
                    let bus = bus.clone();
                    async move {
                        let (fresh, report) = SkillRegistry::discover_in(&root);
                        let count = fresh.len();
                        *skills
                            .write()
                            .map_err(|_| KernelError::command_failed("skills lock poisoned"))? =
                            fresh;

                        for err in &report.errors {
                            bus.publish(Event::new(
                                "skill.load.error",
                                serde_json::json!({ "error": err.to_string() }),
                            ));
                        }
                        bus.publish(Event::new(
                            "skill.reloaded",
                            serde_json::json!({ "count": count, "loaded": report.loaded }),
                        ));

                        let errors: Vec<String> =
                            report.errors.iter().map(|e| e.to_string()).collect();
                        Ok(serde_json::json!({
                            "count": count,
                            "loaded": report.loaded,
                            "errors": errors,
                        }))
                    }
                }),
            )
            .await;
    }

    // skill.import — install a user-supplied skill manifest so it joins the
    // catalogue and persists. Accepts either `{ "path": "<file.json>" }` (read
    // from disk) or `{ "json": "<manifest>" }` (inline). The manifest is
    // validated, written into the writable skills directory as `<name>.json`,
    // and the catalogue is reloaded so the new skill appears immediately.
    {
        let skills = skills.clone();
        let root = root.to_path_buf();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "skill.import",
                Arc::new(move |cmd: Command| {
                    let skills = skills.clone();
                    let root = root.clone();
                    let bus = bus.clone();
                    async move {
                        // Source the manifest text from `json` or `path`.
                        let text = if let Some(json) =
                            cmd.args.get("json").and_then(|j| j.as_str())
                        {
                            json.to_string()
                        } else if let Some(path) = cmd.args.get("path").and_then(|p| p.as_str()) {
                            std::fs::read_to_string(path).map_err(|e| {
                                KernelError::command_failed(format!("cannot read {path}: {e}"))
                            })?
                        } else {
                            return Err(KernelError::command_failed(
                                "skill.import needs a `path` or `json` argument",
                            ));
                        };

                        // Validate before writing anything.
                        let skill = Skill::from_json(&text).map_err(|e| {
                            KernelError::command_failed(format!("invalid skill manifest: {e}"))
                        })?;
                        if skill.name.trim().is_empty() {
                            return Err(KernelError::command_failed(
                                "skill manifest has an empty `name`",
                            ));
                        }

                        // Write it into the writable skills dir under a filename
                        // derived from the (sanitised) skill name.
                        let dir = writable_skills_dir(&root);
                        std::fs::create_dir_all(&dir).map_err(|e| {
                            KernelError::command_failed(format!(
                                "cannot create {}: {e}",
                                dir.display()
                            ))
                        })?;
                        let file = dir.join(format!("{}.json", sanitize_filename(&skill.name)));
                        std::fs::write(&file, skill.to_json()).map_err(|e| {
                            KernelError::command_failed(format!(
                                "cannot write {}: {e}",
                                file.display()
                            ))
                        })?;

                        // Reload so the imported skill is live in the `>` palette.
                        let (fresh, report) = SkillRegistry::discover_in(&root);
                        let count = fresh.len();
                        *skills.write().map_err(|_| {
                            KernelError::command_failed("skills lock poisoned")
                        })? = fresh;
                        bus.publish(Event::new(
                            "skill.reloaded",
                            serde_json::json!({ "count": count, "loaded": report.loaded }),
                        ));
                        bus.publish(Event::new(
                            "skill.imported",
                            serde_json::json!({ "name": skill.name, "path": file.display().to_string() }),
                        ));

                        Ok(serde_json::json!({
                            "name": skill.name,
                            "path": file.display().to_string(),
                            "count": count,
                        }))
                    }
                }),
            )
            .await;
    }

    // mcp.list — the configured MCP servers (with transport, connected state, and
    // tool count) plus the tools currently registered.
    {
        let mcp_config = mcp_config.clone();
        let mcp = mcp.clone();
        let tools = tools.clone();
        kernel
            .commands()
            .register(
                "mcp.list",
                Arc::new(move |_cmd: Command| {
                    let mcp_config = mcp_config.clone();
                    let mcp = mcp.clone();
                    let tools = tools.clone();
                    async move {
                        let connections = mcp.lock().map_err(|_| {
                            KernelError::command_failed("mcp connections lock poisoned")
                        })?;
                        let mcp_config = mcp_config.lock().map_err(|_| {
                            KernelError::command_failed("mcp config lock poisoned")
                        })?;
                        let servers: Vec<serde_json::Value> = mcp_config
                            .servers
                            .iter()
                            .map(|(name, cfg)| {
                                serde_json::json!({
                                    "name": name,
                                    "transport": cfg.transport(),
                                    "command": cfg.command,
                                    "url": cfg.url,
                                    "enabled": cfg.enabled,
                                    "connected": connections.contains(name),
                                    "tool_count": connections.tools_of(name).len(),
                                    "grants": cfg.grants.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                                })
                            })
                            .collect();
                        Ok(serde_json::json!({
                            "servers": servers,
                            "tools": tools.names(),
                        }))
                    }
                }),
            )
            .await;
    }

    // mcp.connect — connect the configured servers and bridge their tools,
    // emitting mcp.connected / mcp.error. With `{ "server": name }`, connects just
    // that one. Idempotent: re-registering a tool of the same name replaces it.
    {
        let mcp_config = mcp_config.clone();
        let mcp = mcp.clone();
        let tools = tools.clone();
        let secrets = secrets.clone();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "mcp.connect",
                Arc::new(move |cmd: Command| {
                    let mcp_config = mcp_config.clone();
                    let mcp = mcp.clone();
                    let tools = tools.clone();
                    let secrets = secrets.clone();
                    let bus = bus.clone();
                    async move {
                        let only = cmd.args.get("server").and_then(|s| s.as_str());
                        // Snapshot the config so the lock isn't held across the
                        // connect await.
                        let config = mcp_config
                            .lock()
                            .map_err(|_| KernelError::command_failed("mcp config lock poisoned"))?
                            .clone();
                        Ok(connect_and_merge(&config, only, &tools, &mcp, &secrets, &bus).await)
                    }
                }),
            )
            .await;
    }

    // mcp.disconnect — drop a server's connection and unregister its tools,
    // emitting mcp.disconnected. Requires `{ "server": name }`.
    {
        let mcp = mcp.clone();
        let tools = tools.clone();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "mcp.disconnect",
                Arc::new(move |cmd: Command| {
                    let mcp = mcp.clone();
                    let tools = tools.clone();
                    let bus = bus.clone();
                    async move {
                        let server = cmd
                            .args
                            .get("server")
                            .and_then(|s| s.as_str())
                            .ok_or_else(|| {
                                KernelError::command_failed("mcp.disconnect needs a `server`")
                            })?
                            .to_string();
                        let removed = mcp
                            .lock()
                            .map_err(|_| {
                                KernelError::command_failed("mcp connections lock poisoned")
                            })?
                            .disconnect(&server, &tools);
                        if let Some(count) = removed {
                            bus.publish(Event::new(
                                "mcp.disconnected",
                                serde_json::json!({ "server": server, "tools": count }),
                            ));
                        }
                        Ok(serde_json::json!({
                            "server": server,
                            "disconnected": removed.is_some(),
                            "tools_removed": removed.unwrap_or(0),
                        }))
                    }
                }),
            )
            .await;
    }

    // mcp.connectors — the built-in connector catalogue: well-known MCP servers a
    // user can add without hand-writing config. Flags which are already
    // configured and which still need a secret.
    {
        let mcp_config = mcp_config.clone();
        kernel
            .commands()
            .register(
                "mcp.connectors",
                Arc::new(move |_cmd: Command| {
                    let mcp_config = mcp_config.clone();
                    async move {
                        let configured: std::collections::BTreeSet<String> = mcp_config
                            .lock()
                            .map_err(|_| {
                                KernelError::command_failed("mcp config lock poisoned")
                            })?
                            .servers
                            .keys()
                            .cloned()
                            .collect();
                        let connectors: Vec<serde_json::Value> = dadhichi_mcp::builtin_connectors()
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "id": c.id,
                                    "description": c.description,
                                    "command": c.command,
                                    "needs_secrets": c.needs_secrets(),
                                    "secrets": c.secrets.iter().map(|s| s.var).collect::<Vec<_>>(),
                                    "grants": c.grants.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                                    "homepage": c.homepage,
                                    "configured": configured.contains(c.id),
                                })
                            })
                            .collect();
                        Ok(serde_json::json!({ "connectors": connectors }))
                    }
                }),
            )
            .await;
    }

    // mcp.add — add a built-in connector by id: materialise its config (scoped to
    // the workspace root), persist it to the writable mcp.json, register it in the
    // live config so the palette shows it, and connect it right away. Args:
    // `{ "connector": "<id>", "name"?: "<local name>", "connect"?: true }`.
    {
        let mcp_config = mcp_config.clone();
        let mcp = mcp.clone();
        let tools = tools.clone();
        let secrets = secrets.clone();
        let bus = kernel.bus().clone();
        let root = root.to_path_buf();
        kernel
            .commands()
            .register(
                "mcp.add",
                Arc::new(move |cmd: Command| {
                    let mcp_config = mcp_config.clone();
                    let mcp = mcp.clone();
                    let tools = tools.clone();
                    let secrets = secrets.clone();
                    let bus = bus.clone();
                    let root = root.clone();
                    async move {
                        let id = cmd
                            .args
                            .get("connector")
                            .and_then(|c| c.as_str())
                            .ok_or_else(|| {
                                KernelError::command_failed("mcp.add needs a `connector` id")
                            })?;
                        let preset = connector(id).ok_or_else(|| {
                            KernelError::command_failed(format!("unknown connector: {id}"))
                        })?;
                        let name = cmd
                            .args
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or(preset.id)
                            .to_string();
                        let should_connect = cmd
                            .args
                            .get("connect")
                            .and_then(|c| c.as_bool())
                            .unwrap_or(true);

                        let server = preset.to_config(&root.to_string_lossy());

                        // Register in the live config (for the palette) and persist
                        // to the writable mcp.json (for the next launch).
                        {
                            let mut guard = mcp_config.lock().map_err(|_| {
                                KernelError::command_failed("mcp config lock poisoned")
                            })?;
                            guard.servers.insert(name.clone(), server.clone());
                        }
                        let path = writable_mcp_config_path(&root);
                        let mut on_disk = McpServersConfig::load_file(&path)
                            .map_err(KernelError::command_failed)?;
                        on_disk.servers.insert(name.clone(), server.clone());
                        on_disk.save(&path).map_err(KernelError::command_failed)?;

                        bus.publish(Event::new(
                            "mcp.added",
                            serde_json::json!({
                                "connector": preset.id,
                                "server": name,
                                "needs_secrets": preset.needs_secrets(),
                                "path": path.display().to_string(),
                            }),
                        ));

                        // Connect it now unless the caller opted out. A missing
                        // secret surfaces as a non-fatal mcp.error in the report.
                        let connect_summary = if should_connect {
                            let mut scoped = McpServersConfig::default();
                            scoped.servers.insert(name.clone(), server);
                            Some(
                                connect_and_merge(
                                    &scoped,
                                    Some(&name),
                                    &tools,
                                    &mcp,
                                    &secrets,
                                    &bus,
                                )
                                .await,
                            )
                        } else {
                            None
                        };

                        Ok(serde_json::json!({
                            "connector": preset.id,
                            "server": name,
                            "needs_secrets": preset.needs_secrets(),
                            "persisted_to": path.display().to_string(),
                            "connect": connect_summary,
                        }))
                    }
                }),
            )
            .await;
    }

    // mcp.resources / mcp.prompts — enumerate the readable resources and prompt
    // templates the connected servers expose, namespaced by server.
    register_mcp_capability_commands(kernel, mcp).await;

    // workspace.reindex — (re)index a path, emitting symbols.updated per file.
    {
        let indexer = indexer.clone();
        kernel
            .commands()
            .register(
                "workspace.reindex",
                Arc::new(move |cmd: Command| {
                    let indexer = indexer.clone();
                    async move {
                        let path = cmd
                            .args
                            .get("path")
                            .and_then(|p| p.as_str())
                            .unwrap_or(".")
                            .to_string();
                        let total = indexer
                            .index_dir(&path)
                            .map_err(KernelError::command_failed)?;
                        Ok(serde_json::json!({ "symbols": total }))
                    }
                }),
            )
            .await;
    }

    // terminal.run — run a shell command through the gated `terminal.run` tool.
    // The tool requires `RunCommands`, set to *interrupt*, so the invocation
    // pauses for approval; the result (or rejection) is published as
    // `terminal.result` / `terminal.error` for the console. Args: `{ command }`.
    {
        let tools = tools.clone();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "terminal.run",
                Arc::new(move |cmd: Command| {
                    let tools = tools.clone();
                    let bus = bus.clone();
                    async move {
                        let command = str_arg(&cmd, "command")?;
                        bus.publish(Event::new(
                            "terminal.started",
                            serde_json::json!({ "command": command }),
                        ));
                        // Grant the permission so the call reaches the approval
                        // gate (rather than being statically denied).
                        let grants = GrantSet::from_iter([Permission::RunCommands]);
                        match tools
                            .invoke(
                                TerminalTool::NAME,
                                serde_json::json!({ "command": command }),
                                &grants,
                            )
                            .await
                        {
                            Ok(result) => {
                                bus.publish(Event::new("terminal.result", result.clone()));
                                Ok(result)
                            }
                            Err(err) => {
                                bus.publish(Event::new(
                                    "terminal.error",
                                    serde_json::json!({ "command": command, "error": err.to_string() }),
                                ));
                                Err(KernelError::command_failed(err))
                            }
                        }
                    }
                }),
            )
            .await;
    }

    // editor.save — emit a save event the status bar reflects.
    {
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "editor.save",
                Arc::new(move |_cmd: Command| {
                    let bus = bus.clone();
                    async move {
                        bus.publish(dadhichi_core::Event::new(
                            "editor.saved",
                            serde_json::json!({ "ok": true }),
                        ));
                        Ok(serde_json::json!({ "saved": true }))
                    }
                }),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_index::store::SymbolStore;

    #[tokio::test]
    async fn palette_is_seeded_from_registered_commands() {
        let ctrl = AppController::new(".").await;
        let commands = ctrl.ui().palette.results();
        let names: Vec<_> = commands.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"agent.run"));
        assert!(names.contains(&"workspace.reindex"));
        // The new skill/connector management commands are registered too.
        assert!(names.contains(&"skill.import"));
        assert!(names.contains(&"mcp.connectors"));
        assert!(names.contains(&"mcp.add"));
        assert!(names.contains(&"agent.spawn"));
    }

    #[tokio::test]
    async fn save_active_document_writes_the_buffer_to_disk() {
        let dir = std::env::temp_dir().join(format!("dadhichi-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "original").unwrap();

        let mut ctrl = AppController::new(&dir).await;
        ctrl.ui_mut().open_document(Some(file.clone()), "original");
        // Edit the buffer, then save it.
        if let Some(doc) = ctrl.ui_mut().active_document_mut() {
            doc.insert(" edited");
        }
        assert!(ctrl.ui().active_document().unwrap().dirty);

        let saved = ctrl.save_active_document().unwrap();
        assert!(saved, "a buffer with a path saves");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), " editedoriginal");
        assert!(
            !ctrl.ui().active_document().unwrap().dirty,
            "dirty flag cleared after save"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_active_document_noops_without_a_path() {
        let mut ctrl = AppController::new(".").await;
        ctrl.ui_mut().open_document(None, "scratch");
        assert!(
            !ctrl.save_active_document().unwrap(),
            "a scratch buffer with no path is not saved"
        );
    }

    #[tokio::test]
    async fn unknown_specialist_delegation_reports_the_roster() {
        let mut ctrl = AppController::new(".").await;
        ctrl.start_delegation("bogus-agent", "do something");
        ctrl.pump();
        assert!(
            ctrl.ui()
                .chat
                .iter()
                .any(|l| l.contains("unknown specialist") && l.contains("code-agent")),
            "unknown specialist is reported with the roster: {:?}",
            ctrl.ui().chat
        );
        // No review was staged, and resolving is a safe no-op.
        assert!(!ctrl.has_pending_delegation());
        ctrl.resolve_delegation(false);
    }

    #[tokio::test]
    async fn task_tool_is_registered_and_lists_specialists() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        assert!(has_tool(&out, "task"), "task tool registered: {out}");
    }

    #[tokio::test]
    async fn fs_and_memory_tools_are_registered() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        for tool in [
            "fs.read",
            "fs.write",
            "fs.ls",
            "memory.write",
            "memory.recall",
        ] {
            assert!(has_tool(&out, tool), "{tool} registered: {out}");
        }
    }

    #[tokio::test]
    async fn agent_reads_and_writes_its_sandboxed_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl = AppController::new(dir.path()).await;

        // Subscribe to the approval topic *before* invoking, so the prompt can't
        // race ahead of the waiter.
        let mut sub = ctrl.kernel.bus().subscribe_topic("agent.approval");
        let approvals = ctrl.approvals.clone();
        let tools = ctrl.tools().clone();

        // A write needs WriteWorkspace, which is set to interrupt.
        let write = tokio::spawn(async move {
            tools
                .invoke(
                    "fs.write",
                    serde_json::json!({ "path": "notes/plan.md", "content": "step 1" }),
                    &GrantSet::from_iter([Permission::WriteWorkspace]),
                )
                .await
        });

        // Receive the interrupt and approve it.
        let ev = sub.recv().await.unwrap();
        let id = ev.payload["id"].as_str().unwrap();
        let uuid = uuid::Uuid::parse_str(id).unwrap();
        let tx = approvals.lock().unwrap().remove(&uuid).unwrap();
        tx.send(Decision::Approve).unwrap();
        write.await.unwrap().unwrap();

        // It really landed inside the sandbox root.
        assert!(dir.path().join("notes/plan.md").exists());

        // A read (ReadWorkspace, un-gated) returns it — the grant a delegate has.
        let read = ctrl
            .tools()
            .invoke(
                "fs.read",
                serde_json::json!({ "path": "notes/plan.md" }),
                &GrantSet::from_iter([Permission::ReadWorkspace]),
            )
            .await
            .unwrap();
        assert_eq!(read["content"], "step 1");
    }

    #[tokio::test]
    async fn agent_spawn_delegates_to_an_isolated_specialist() {
        let mut ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch(
                "agent.spawn",
                serde_json::json!({ "agent": "test-agent", "goal": "cover the parser" }),
            )
            .await
            .unwrap();
        assert_eq!(out["subagent_type"], "test-agent");
        assert_eq!(out["status"], "Completed");
        assert!(
            out["summary"]
                .as_str()
                .unwrap()
                .contains("cover the parser")
        );

        // The delegation surfaced in the console via agent.delegated.
        ctrl.pump();
        assert!(
            ctrl.ui().chat.iter().any(|l| l.contains("agent.delegated")),
            "chat: {:?}",
            ctrl.ui().chat
        );
    }

    #[tokio::test]
    async fn agent_spawn_rejects_unknown_specialist() {
        let ctrl = AppController::new(".").await;
        let err = ctrl
            .dispatch(
                "agent.spawn",
                serde_json::json!({ "agent": "ghost-agent", "goal": "x" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost-agent") || err.to_string().contains("unknown"));
    }

    #[tokio::test]
    async fn mcp_connectors_surfaces_the_builtin_catalogue() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("mcp.connectors", serde_json::json!({}))
            .await
            .unwrap();
        let ids: Vec<&str> = out["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"github"), "catalogue lists github: {ids:?}");
        assert!(ids.contains(&"filesystem"));
        // GitHub declares a required secret; none are configured yet.
        let github = out["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "github")
            .unwrap();
        assert_eq!(github["needs_secrets"], true);
        assert_eq!(github["configured"], false);
    }

    #[tokio::test]
    async fn at_palette_offers_catalogue_connectors_as_add_actions() {
        // With no configured servers, the `@` palette still lists the built-in
        // connectors, each an "add" action.
        let ctrl = AppController::new(".").await;
        let mut app = App::new();
        app.palette.set_mcp_servers(mcp_entries(
            &ctrl.mcp_config.lock().unwrap(),
            &ctrl.mcp.lock().unwrap(),
        ));
        app.palette.open();
        app.palette.push('@');
        let fs = app
            .palette
            .items()
            .into_iter()
            .find(|i| i.label() == "filesystem")
            .expect("filesystem connector offered");
        assert!(fs.detail().unwrap().starts_with("add · "));
    }

    #[tokio::test]
    async fn dispatching_agent_run_streams_events_into_the_console() {
        let mut ctrl = AppController::new(".").await;

        let result = ctrl
            .dispatch(
                "agent.run",
                serde_json::json!({ "goal": "explain ownership" }),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "Completed");

        // The agent's progress events are now buffered on the bus; pumping
        // routes them into the Agent Console panel.
        let applied = ctrl.pump();
        assert!(applied > 0);
        assert!(
            ctrl.ui().chat.iter().any(|line| line.contains("agent.")),
            "chat: {:?}",
            ctrl.ui().chat
        );
    }

    #[tokio::test]
    async fn start_agent_goal_runs_without_blocking_and_streams_events() {
        let mut ctrl = AppController::new(".").await;
        // Returns immediately (spawns the run); the render loop would keep going.
        ctrl.ui_mut().set_agent_running(true);
        ctrl.start_agent_goal("explain ownership");

        // Drain events as they arrive, yielding to let the spawned task progress.
        // The tool-using ReAct agent emits its events across several phases
        // (planning → running → completed), so keep pumping until the run reaches
        // its terminal state rather than stopping at the first `agent.` line.
        let mut saw_agent_event = false;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ctrl.pump();
            if ctrl.ui().chat.iter().any(|l| l.contains("agent.")) {
                saw_agent_event = true;
            }
            // Stop once the busy flag clears — the terminal status has landed.
            if saw_agent_event && !ctrl.ui().agent_running {
                break;
            }
        }
        assert!(saw_agent_event, "chat: {:?}", ctrl.ui().chat);
        // A terminal status cleared the busy flag.
        assert!(!ctrl.ui().agent_running, "chat: {:?}", ctrl.ui().chat);
    }

    #[tokio::test]
    async fn shell_command_interrupts_for_approval_then_runs_when_approved() {
        let mut ctrl = AppController::new(".").await;
        ctrl.start_terminal("echo approved-run");

        // The interrupt surfaces as an agent.approval event → a pending prompt.
        let mut prompt_id = None;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ctrl.pump();
            if let Some(p) = ctrl.ui().pending_approval() {
                prompt_id = Some(p.id.clone());
                break;
            }
        }
        let id = prompt_id.expect("approval prompt was raised");

        // Approve it; the parked command resumes and reports its output.
        ctrl.resolve_approval(&id, Decision::Approve);
        let mut saw_result = false;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ctrl.pump();
            if ctrl.ui().chat.iter().any(|l| l.contains("approved-run")) {
                saw_result = true;
                break;
            }
        }
        assert!(saw_result, "chat: {:?}", ctrl.ui().chat);
        // The prompt was cleared by the resolved event.
        assert!(ctrl.ui().pending_approval().is_none());
    }

    #[tokio::test]
    async fn shell_command_is_rejected_when_denied() {
        let mut ctrl = AppController::new(".").await;
        ctrl.start_terminal("echo should-not-appear");

        let mut prompt_id = None;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ctrl.pump();
            if let Some(p) = ctrl.ui().pending_approval() {
                prompt_id = Some(p.id.clone());
                break;
            }
        }
        let id = prompt_id.expect("approval prompt was raised");

        ctrl.resolve_approval(&id, Decision::Deny);
        let mut saw_error = false;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ctrl.pump();
            if ctrl.ui().chat.iter().any(|l| l.contains("terminal.error")) {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error, "chat: {:?}", ctrl.ui().chat);
        // The command never ran, so no successful result was published (the
        // command name still echoes in the started/error lines, so we key on the
        // result event, not the substring).
        assert!(
            !ctrl.ui().chat.iter().any(|l| l.contains("terminal.result")),
            "denied command must not execute: {:?}",
            ctrl.ui().chat
        );
    }

    #[tokio::test]
    async fn reindex_command_updates_symbols_and_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.rs"), "fn alpha() {}\nstruct Beta;").unwrap();

        let mut ctrl = AppController::new(dir.path()).await;
        let out = ctrl
            .dispatch(
                "workspace.reindex",
                serde_json::json!({ "path": dir.path().to_str().unwrap() }),
            )
            .await
            .unwrap();
        assert_eq!(out["symbols"], 2);

        // The store now answers a go-to-definition query.
        assert_eq!(ctrl.store().definitions("alpha").unwrap().len(), 1);

        // And the UI status bar reflected the indexing via a symbols.updated event.
        ctrl.pump();
        assert!(
            ctrl.ui().status.contains("indexed"),
            "status: {}",
            ctrl.ui().status
        );
    }

    #[tokio::test]
    async fn palette_selection_dispatches() {
        let mut ctrl = AppController::new(".").await;
        ctrl.ui_mut().palette.open();
        // Type to select `editor.save` deterministically.
        for c in "editor.save".chars() {
            ctrl.ui_mut().palette.push(c);
        }
        let ran = ctrl.run_palette_selection().await;
        assert_eq!(ran.as_deref(), Some("editor.save"));
        assert!(!ctrl.ui().palette.is_open());
    }

    #[tokio::test]
    async fn palette_skill_mode_runs_a_skill() {
        let mut ctrl = AppController::new(".").await;
        ctrl.ui_mut().palette.open();
        // `>` switches to skill mode; type to select `code-review`.
        for c in ">code-review".chars() {
            ctrl.ui_mut().palette.push(c);
        }
        assert!(ctrl.ui().palette.in_skill_mode());
        // The highlighted row is the skill, carrying its capability summary.
        let items = ctrl.ui().palette.items();
        assert_eq!(items[0].label(), "code-review");
        assert!(items[0].detail().unwrap().contains("read_workspace"));

        // Accepting it runs the skill via `skill.run`, streaming skill.* events.
        let ran = ctrl.run_palette_selection().await;
        assert_eq!(ran.as_deref(), Some("skill.run:code-review"));
        assert!(ctrl.pump() > 0);
        assert!(ctrl.ui().chat.iter().any(|l| l.contains("skill.")));
    }

    #[tokio::test]
    async fn skills_are_registered_and_runnable() {
        let mut ctrl = AppController::new(".").await;
        // The built-in library is available to a frontend.
        assert!(ctrl.skills().read().unwrap().contains("explain"));

        // Running a skill through the command dispatches its `skill:` agent and
        // streams `skill.*` progress into the console.
        let out = ctrl
            .dispatch(
                "skill.run",
                serde_json::json!({ "skill": "explain", "goal": "what is a kernel" }),
            )
            .await
            .unwrap();
        assert_eq!(out["skill"], "explain");
        assert_eq!(out["status"], "Completed");

        let applied = ctrl.pump();
        assert!(applied > 0);
        assert!(
            ctrl.ui().chat.iter().any(|line| line.contains("skill.")),
            "chat: {:?}",
            ctrl.ui().chat
        );
    }

    #[tokio::test]
    async fn unknown_skill_is_rejected() {
        let ctrl = AppController::new(".").await;
        let err = ctrl
            .dispatch("skill.run", serde_json::json!({ "skill": "nope" }))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn skill_list_returns_specs() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("skill.list", serde_json::json!({}))
            .await
            .unwrap();

        assert!(out["count"].as_u64().unwrap() >= 5);
        let specs = out["skills"].as_array().unwrap();
        // Each spec carries the discovery metadata a picker needs.
        let explain = specs
            .iter()
            .find(|s| s["name"] == "explain")
            .expect("explain skill listed");
        assert!(explain["description"].is_string());
        assert!(explain["permissions"].is_array());
        assert!(explain["tools"].is_object());
        assert!(explain["steps"].is_number());

        // A skill that declares a permission surfaces it in the spec.
        let review = specs.iter().find(|s| s["name"] == "code-review").unwrap();
        assert_eq!(review["permissions"][0], "read_workspace");
    }

    #[tokio::test]
    async fn project_local_skill_manifest_is_discovered_and_runnable() {
        // A project skill dropped into <root>/.dadhichi/skills is loaded when
        // the workspace opens, and is runnable like any built-in.
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".dadhichi").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("house-style.json"),
            r#"{"name":"house-style","description":"Apply our house style"}"#,
        )
        .unwrap();

        let ctrl = AppController::new(root.path()).await;
        assert!(ctrl.skills().read().unwrap().contains("house-style"));

        let out = ctrl
            .dispatch(
                "skill.run",
                serde_json::json!({ "skill": "house-style", "goal": "tidy up" }),
            )
            .await
            .unwrap();
        assert_eq!(out["skill"], "house-style");
        assert_eq!(out["status"], "Completed");
    }

    #[tokio::test]
    async fn skill_reload_picks_up_new_manifests_live() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".dadhichi").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Boot with no project skills present.
        let ctrl = AppController::new(root.path()).await;
        assert!(!ctrl.skills().read().unwrap().contains("late-arrival"));
        assert!(
            ctrl.dispatch("skill.run", serde_json::json!({ "skill": "late-arrival" }))
                .await
                .is_err()
        );

        // Author a new skill after boot, then reload.
        std::fs::write(
            skill_dir.join("late-arrival.json"),
            r#"{"name":"late-arrival","description":"added at runtime"}"#,
        )
        .unwrap();
        let report = ctrl
            .dispatch("skill.reload", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(report["loaded"], serde_json::json!(["late-arrival"]));

        // It is now live: catalogued and runnable without a restart.
        assert!(ctrl.skills().read().unwrap().contains("late-arrival"));
        let out = ctrl
            .dispatch("skill.run", serde_json::json!({ "skill": "late-arrival" }))
            .await
            .unwrap();
        assert_eq!(out["status"], "Completed");
    }

    #[tokio::test]
    async fn skill_reload_reports_malformed_manifests() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".dadhichi").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("broken.json"), "{ not json").unwrap();

        let ctrl = AppController::new(root.path()).await;
        let report = ctrl
            .dispatch("skill.reload", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
        // The built-ins are still present after a reload with a bad manifest.
        assert!(report["count"].as_u64().unwrap() >= 5);
    }

    #[tokio::test]
    async fn file_watcher_hot_reloads_and_refreshes_the_palette() {
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".dadhichi").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Boot with the watcher armed over an (empty) project skills dir.
        let mut ctrl = AppController::new(root.path()).await;
        assert!(!ctrl.skills().read().unwrap().contains("hot"));

        // Author a manifest on disk — no command dispatched. The watcher should
        // notice, reload the catalogue, and publish `skill.reloaded`.
        std::fs::write(
            skill_dir.join("hot.json"),
            r#"{"name":"hot","description":"appeared on disk"}"#,
        )
        .unwrap();

        // Poll until the catalogue actually contains the new skill, pumping the
        // bus each tick. We wait on the real condition (not just the first
        // `skill.reloaded`), because a create-then-write can trigger an early
        // reload before the file's contents have landed.
        let mut loaded = false;
        for _ in 0..100 {
            ctrl.pump();
            if ctrl.skills().read().unwrap().contains("hot") {
                loaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(loaded, "watcher did not load the new skill");
        // A final pump processes the reload event that carried `hot`, refreshing
        // the palette.
        ctrl.pump();
        assert!(ctrl.ui().chat.iter().any(|l| l.contains("skill.reloaded")));

        // The palette's `>` picker reflects it, live.
        ctrl.ui_mut().palette.open();
        ctrl.ui_mut().palette.push('>');
        assert!(
            ctrl.ui()
                .palette
                .items()
                .iter()
                .any(|item| item.label() == "hot"),
            "palette skill list not refreshed after watcher reload"
        );
    }

    #[tokio::test]
    async fn mcp_config_is_discovered_and_listed() {
        // A project mcp.json declares a server. Booting parses it and attempts
        // to connect; the bogus command fails non-fatally, but the server is
        // still catalogued and surfaced by `mcp.list`.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dadhichi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mcp.json"),
            r#"{"servers":{"github":{
                "command":"definitely-not-a-real-binary-xyz",
                "args":["-y","server"],
                "grants":["network"]
            }}}"#,
        )
        .unwrap();

        let mut ctrl = AppController::new(root.path()).await;

        // The failed connect surfaced as an mcp.error in the console.
        assert!(ctrl.pump() > 0);
        assert!(ctrl.ui().chat.iter().any(|l| l.contains("mcp.error")));

        // mcp.list reflects the configured server and its permission envelope.
        let out = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        let servers = out["servers"].as_array().unwrap();
        let gh = servers.iter().find(|s| s["name"] == "github").unwrap();
        assert_eq!(gh["command"], "definitely-not-a-real-binary-xyz");
        assert_eq!(gh["transport"], "stdio");
        assert_eq!(gh["grants"][0], "network");
        assert!(gh["enabled"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn mcp_list_surfaces_remote_server_transport() {
        // A hosted (URL) server is catalogued with its remote transport. The
        // connect attempt fails offline (no such host), non-fatally.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dadhichi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mcp.json"),
            r#"{"servers":{"linear":{
                "url":"wss://mcp.invalid.example/socket",
                "headers":{"Authorization":"Bearer ${env:NOPE}"},
                "grants":["network"]
            }}}"#,
        )
        .unwrap();

        let ctrl = AppController::new(root.path()).await;
        let out = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        let servers = out["servers"].as_array().unwrap();
        let linear = servers.iter().find(|s| s["name"] == "linear").unwrap();
        assert_eq!(linear["transport"], "websocket");
        assert_eq!(linear["url"], "wss://mcp.invalid.example/socket");
    }

    #[tokio::test]
    async fn mcp_list_is_empty_without_config() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        assert!(out["servers"].as_array().unwrap().is_empty());
        // The built-in echo tool is always present.
        assert!(out["tools"].as_array().unwrap().iter().any(|t| t == "echo"));
    }

    #[test]
    fn vault_file_round_trips_through_load_vault() {
        // Persist an encrypted vault, then reopen it via the app loader.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mut vault = Vault::new("s3cret-pass");
        vault.put("github_token", "ghp_from_vault").unwrap();
        std::fs::write(&path, serde_json::to_string(vault.data()).unwrap()).unwrap();

        let resolver = SecretResolver::with_vault(load_vault(&path, "s3cret-pass").unwrap());
        assert_eq!(
            resolver.resolve("vault:github_token").as_deref(),
            Some("ghp_from_vault")
        );

        // A missing file yields no vault (env-only resolution).
        assert!(load_vault(&dir.path().join("absent.json"), "x").is_none());
    }

    #[tokio::test]
    async fn vault_secret_is_injected_into_a_connector() {
        // A ${vault:...} reference in mcp.json is resolved from the vault before
        // the server is launched. The command is bogus so the launch fails — but
        // crucially NOT with an "unresolved secret" error, proving the token was
        // injected.
        let mut vault = Vault::new("pw");
        vault.put("gh_token", "ghp_secret").unwrap();
        let resolver = SecretResolver::with_vault(vault);

        let cfg = McpServersConfig::from_json(
            r#"{"servers":{"github":{
                "command":"definitely-not-a-real-binary-xyz",
                "env":{"GITHUB_TOKEN":"${vault:gh_token}"},
                "grants":["network"]
            }}}"#,
        )
        .unwrap();
        let registry = ToolRegistry::new();
        let (_conns, report) = connect_servers(&cfg, &registry, |key| resolver.resolve(key)).await;

        assert_eq!(report.errors.len(), 1);
        assert!(
            !report.errors[0].message.contains("unresolved"),
            "vault secret was not resolved: {}",
            report.errors[0].message
        );
    }

    #[tokio::test]
    async fn missing_vault_secret_fails_the_connector_cleanly() {
        // With no vault, a ${vault:...} reference cannot resolve: the server is
        // rejected before launch with an unresolved-secret error, never run with
        // a blank credential.
        let resolver = SecretResolver::new();
        let cfg = McpServersConfig::from_json(
            r#"{"servers":{"github":{
                "command":"echo",
                "env":{"GITHUB_TOKEN":"${vault:gh_token}"}
            }}}"#,
        )
        .unwrap();
        let registry = ToolRegistry::new();
        let (conns, report) = connect_servers(&cfg, &registry, |key| resolver.resolve(key)).await;

        assert!(conns.names().is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("gh_token"));
    }

    #[tokio::test]
    async fn mcp_disconnect_of_unknown_server_is_a_noop() {
        let ctrl = AppController::new(".").await;
        let out = ctrl
            .dispatch("mcp.disconnect", serde_json::json!({ "server": "ghost" }))
            .await
            .unwrap();
        assert_eq!(out["disconnected"], false);
        assert_eq!(out["tools_removed"], 0);
    }

    #[tokio::test]
    async fn at_palette_lists_configured_servers_even_when_offline() {
        // A configured server whose (bogus) command fails to launch still shows
        // in the `@` palette, marked disconnected.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dadhichi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mcp.json"),
            r#"{"servers":{"github":{"command":"definitely-not-a-real-binary-xyz"}}}"#,
        )
        .unwrap();

        let mut ctrl = AppController::new(root.path()).await;
        ctrl.ui_mut().palette.open();
        ctrl.ui_mut().palette.push('@');
        let items = ctrl.ui().palette.items();
        let gh = items.iter().find(|i| i.label() == "github").unwrap();
        assert!(gh.detail().unwrap().contains("offline"));
    }

    /// A loopback WebSocket MCP server exposing one tool, one resource, and one
    /// prompt — enough to drive the whole app-level lifecycle offline.
    async fn spawn_mock_mcp_server() -> String {
        use futures::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let req: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                        let id = req.get("id").cloned().unwrap();
                        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let result = match method {
                            "initialize" => serde_json::json!({
                                "capabilities": { "tools": {}, "resources": {}, "prompts": {} }
                            }),
                            "tools/list" => serde_json::json!({
                                "tools": [{ "name": "send_message", "description": "post", "inputSchema": {} }]
                            }),
                            "resources/list" => serde_json::json!({
                                "resources": [{ "uri": "slack://general", "name": "general" }]
                            }),
                            "resources/read" => serde_json::json!({
                                "contents": [{ "uri": "slack://general", "text": "hello" }]
                            }),
                            "prompts/list" => serde_json::json!({
                                "prompts": [{ "name": "standup", "description": "daily" }]
                            }),
                            "prompts/get" => serde_json::json!({
                                "messages": [{ "role": "user", "content": { "type": "text", "text": "x" } }]
                            }),
                            _ => serde_json::Value::Null,
                        };
                        let resp =
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                        ws.send(Message::text(resp.to_string())).await.unwrap();
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn mcp_connection_lifecycle_end_to_end() {
        let url = spawn_mock_mcp_server().await;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dadhichi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mcp.json"),
            format!(r#"{{"servers":{{"slack":{{"url":"{url}","grants":["network"]}}}}}}"#),
        )
        .unwrap();

        let mut ctrl = AppController::new(root.path()).await;

        // Connected at boot: mcp.list reports it, with its bridged tool.
        let list = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        let slack = find_server(&list, "slack");
        assert_eq!(slack["connected"], true);
        assert_eq!(slack["tool_count"], 1);
        assert_eq!(slack["transport"], "websocket");
        assert!(has_tool(&list, "slack.send_message"));

        // The `@` palette shows it connected.
        ctrl.ui_mut().palette.open();
        ctrl.ui_mut().palette.push('@');
        let entry = ctrl
            .ui()
            .palette
            .items()
            .into_iter()
            .find(|i| i.label() == "slack")
            .unwrap();
        assert!(entry.detail().unwrap().contains("connected"));
        ctrl.ui_mut().palette.close();

        // Resources and prompts are reachable through the bridged connection.
        let resources = ctrl
            .dispatch("mcp.resources", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resources["resources"][0]["uri"], "slack://general");
        let read = ctrl
            .dispatch(
                "mcp.resource.read",
                serde_json::json!({ "server": "slack", "uri": "slack://general" }),
            )
            .await
            .unwrap();
        assert_eq!(read["contents"]["contents"][0]["text"], "hello");

        let prompts = ctrl
            .dispatch("mcp.prompts", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(prompts["prompts"][0]["name"], "standup");
        let got = ctrl
            .dispatch(
                "mcp.prompt.get",
                serde_json::json!({ "server": "slack", "name": "standup" }),
            )
            .await
            .unwrap();
        assert!(got["prompt"]["messages"].is_array());

        // Disconnect: the connection drops and its tool is unregistered.
        let dis = ctrl
            .dispatch("mcp.disconnect", serde_json::json!({ "server": "slack" }))
            .await
            .unwrap();
        assert_eq!(dis["disconnected"], true);
        assert_eq!(dis["tools_removed"], 1);

        let list2 = ctrl
            .dispatch("mcp.list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(find_server(&list2, "slack")["connected"], false);
        assert!(!has_tool(&list2, "slack.send_message"));

        // A prompt call now fails cleanly — the server is no longer connected.
        let err = ctrl
            .dispatch(
                "mcp.prompt.get",
                serde_json::json!({ "server": "slack", "name": "standup" }),
            )
            .await;
        assert!(err.is_err());

        // Reconnect just that server via the targeted command.
        let re = ctrl
            .dispatch("mcp.connect", serde_json::json!({ "server": "slack" }))
            .await
            .unwrap();
        assert_eq!(re["connected"], serde_json::json!(["slack"]));
        assert!(has_tool(
            &ctrl
                .dispatch("mcp.list", serde_json::json!({}))
                .await
                .unwrap(),
            "slack.send_message"
        ));
    }

    fn find_server<'a>(list: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        list["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()
    }

    fn has_tool(list: &serde_json::Value, tool: &str) -> bool {
        list["tools"].as_array().unwrap().iter().any(|t| t == tool)
    }
}
