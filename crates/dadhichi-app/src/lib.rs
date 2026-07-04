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

use dadhichi_agent::{AgentContext, Orchestrator, SpecialistAgent, agents::ConversationalAgent};
use dadhichi_ai::{MockProvider, ModelRouter};
use dadhichi_core::{Command, Kernel, KernelError, RecvError, Subscription};
use dadhichi_index::Indexer;
use dadhichi_index::store::SqliteSymbolStore;
use dadhichi_mcp::{EchoTool, GrantSet, Permission, ToolRegistry};
use dadhichi_ui::App;
use std::path::PathBuf;
use std::sync::Arc;

/// Owns the live IDE state and mediates between the frontend and the kernel.
pub struct AppController {
    kernel: Kernel,
    ui: App,
    events: Subscription,
    store: Arc<SqliteSymbolStore>,
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

        // Core services, shared with the command handlers.
        let router = {
            let mut r = ModelRouter::new();
            r.register(Arc::new(MockProvider::default()));
            Arc::new(r)
        };
        let tools = {
            let mut t = ToolRegistry::new();
            t.register(Arc::new(EchoTool));
            Arc::new(t)
        };
        let store = Arc::new(SqliteSymbolStore::in_memory().expect("open symbol store"));
        let indexer = Indexer::new(store.clone()).with_event_bus(kernel.bus().clone());

        let orchestrator = {
            let mut orch = Orchestrator::new();
            orch.register(Arc::new(ConversationalAgent::default()));
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
            Arc::new(orch)
        };

        register_commands(&kernel, &router, &tools, &orchestrator, &indexer).await;

        // Build the UI and seed the palette from the registered command names.
        let mut ui = App::new();
        ui.open_workspace(&root);
        ui.set_commands(kernel.commands().command_names().await);

        let events = kernel.bus().subscribe();
        Self {
            kernel,
            ui,
            events,
            store,
        }
    }

    /// Immutable access to the UI view-models.
    pub fn ui(&self) -> &App {
        &self.ui
    }

    /// Mutable access to the UI view-models (for the frontend's input handling).
    pub fn ui_mut(&mut self) -> &mut App {
        &mut self.ui
    }

    /// The symbol store, for code-intelligence queries.
    pub fn store(&self) -> &Arc<SqliteSymbolStore> {
        &self.store
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

    /// Accept the highlighted command-palette entry, close the palette, and
    /// dispatch it. Returns the dispatched command name, if any.
    pub async fn run_palette_selection(&mut self) -> Option<String> {
        let name = self.ui.palette.accept()?;
        self.ui.palette.close();
        let _ = self.dispatch(&name, serde_json::json!({})).await;
        Some(name)
    }

    /// Drain all currently-buffered bus events into the UI view-models. Returns
    /// how many were applied. Call once per frame — it never blocks.
    pub fn pump(&mut self) -> usize {
        let mut applied = 0;
        loop {
            match self.events.try_recv() {
                Ok(Some(event)) => {
                    self.ui.apply_event(&event);
                    applied += 1;
                }
                Ok(None) => break,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        applied
    }
}

/// Register the IDE's commands. Each runs real work and emits bus events, so the
/// UI updates purely by pumping the bus.
async fn register_commands(
    kernel: &Kernel,
    router: &Arc<ModelRouter>,
    tools: &Arc<ToolRegistry>,
    orchestrator: &Arc<Orchestrator>,
    indexer: &Indexer,
) {
    // agent.run — run an agent against a goal; its progress streams as agent.*.
    {
        let router = router.clone();
        let tools = tools.clone();
        let orchestrator = orchestrator.clone();
        let bus = kernel.bus().clone();
        kernel
            .commands()
            .register(
                "agent.run",
                Arc::new(move |cmd: Command| {
                    let router = router.clone();
                    let tools = tools.clone();
                    let orchestrator = orchestrator.clone();
                    let bus = bus.clone();
                    async move {
                        let goal = cmd
                            .args
                            .get("goal")
                            .and_then(|g| g.as_str())
                            .unwrap_or("Explain what makes Dadhichi agent-native.")
                            .to_string();
                        let agent = cmd
                            .args
                            .get("agent")
                            .and_then(|a| a.as_str())
                            .unwrap_or("conversational-agent")
                            .to_string();

                        let mut ctx = AgentContext::new(
                            router,
                            tools,
                            GrantSet::from_iter([Permission::ReadWorkspace]),
                            bus,
                        );
                        let outcome = orchestrator
                            .run(&agent, &goal, &mut ctx)
                            .await
                            .map_err(KernelError::command_failed)?;
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
}
