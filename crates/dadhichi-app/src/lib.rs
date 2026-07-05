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
    Agent, AgentContext, Orchestrator, SpecialistAgent, agents::ConversationalAgent,
};
use dadhichi_ai::{ModelRouter, ProviderPlan};
use dadhichi_core::{Command, Event, Kernel, KernelError, RecvError, Subscription};
use dadhichi_index::Indexer;
use dadhichi_index::store::SqliteSymbolStore;
use dadhichi_mcp::{EchoTool, GrantSet, Permission, ToolRegistry};
use dadhichi_skill::{
    SharedSkills, SkillAgent, SkillRegistry, SkillSpec, SkillTools, SkillWatchGuard, shared,
    watch_skills,
};
use dadhichi_ui::{App, PaletteAction, SkillEntry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Owns the live IDE state and mediates between the frontend and the kernel.
pub struct AppController {
    kernel: Kernel,
    ui: App,
    events: Subscription,
    store: Arc<SqliteSymbolStore>,
    skills: SharedSkills,
    /// Keeps the skill-manifest file watch alive; dropping it stops watching.
    _skill_watch: Option<SkillWatchGuard>,
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
        let tools = {
            let mut t = ToolRegistry::new();
            t.register(Arc::new(EchoTool));
            Arc::new(t)
        };
        let store = Arc::new(SqliteSymbolStore::in_memory().expect("open symbol store"));
        let indexer = Indexer::new(store.clone()).with_event_bus(kernel.bus().clone());

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

        let orchestrator = {
            let mut orch = Orchestrator::new();
            orch.register(Arc::new(ConversationalAgent::new(&model_id)));
            for agent in [
                SpecialistAgent::code(),
                SpecialistAgent::refactor(),
                SpecialistAgent::test(),
                SpecialistAgent::docs(),
                SpecialistAgent::review(),
                SpecialistAgent::git(),
                SpecialistAgent::security(),
            ] {
                orch.register(Arc::new(agent.with_model(&model_id)));
            }
            Arc::new(orch)
        };

        register_commands(
            &kernel,
            &router,
            &tools,
            &orchestrator,
            &skills,
            &model_id,
            &root,
            &indexer,
        )
        .await;

        // Build the UI and seed the palette from the registered command names
        // (default mode) and the skill catalogue (the `>` skill mode).
        let mut ui = App::new();
        ui.open_workspace(&root);
        ui.set_commands(kernel.commands().command_names().await);
        ui.palette
            .set_skills(skill_entries(&skills.read().expect("skills lock")));

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

        Self {
            kernel,
            ui,
            events,
            store,
            skills,
            _skill_watch: skill_watch,
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
        };
        // A command may have changed the skill set (e.g. `skill.reload`); keep
        // the palette's skill list current.
        self.refresh_skills();
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

    /// Drain all currently-buffered bus events into the UI view-models. Returns
    /// how many were applied. Call once per frame — it never blocks.
    pub fn pump(&mut self) -> usize {
        let mut applied = 0;
        let mut skills_changed = false;
        loop {
            match self.events.try_recv() {
                Ok(Some(event)) => {
                    // The file watcher reloads the catalogue off-thread and
                    // announces it here; refresh the palette's skill list so the
                    // `>` picker reflects the change without a keystroke.
                    if event.topic.as_str() == "skill.reloaded" {
                        skills_changed = true;
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
        applied
    }
}

/// Register the IDE's commands. Each runs real work and emits bus events, so the
/// UI updates purely by pumping the bus.
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

#[allow(clippy::too_many_arguments)]
async fn register_commands(
    kernel: &Kernel,
    router: &Arc<ModelRouter>,
    tools: &Arc<ToolRegistry>,
    orchestrator: &Arc<Orchestrator>,
    skills: &SharedSkills,
    model_id: &str,
    root: &Path,
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

        // Poll pump() until the reload event lands (bounded, watchers are async).
        let mut reloaded = false;
        for _ in 0..100 {
            if ctrl.pump() > 0 && ctrl.ui().chat.iter().any(|l| l.contains("skill.reloaded")) {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(reloaded, "watcher did not publish skill.reloaded");

        // The catalogue and the palette's `>` picker both reflect it, live.
        assert!(ctrl.skills().read().unwrap().contains("hot"));
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
}
