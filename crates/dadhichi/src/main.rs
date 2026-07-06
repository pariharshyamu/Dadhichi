//! # Dadhichi
//!
//! The runtime entry point. It boots the [microkernel](dadhichi_core::Kernel),
//! registers the core services (AI model router, tool registry, workspace),
//! wires the Agent Console to the event bus, and runs a demonstration agent.
//!
//! This binary is the *minimum viable IDE*: a headless kernel that can plan and
//! execute an agent run end to end. The GUI shells (Slint/GPUI) and the LSP,
//! DAP, terminal, and git services attach to this same kernel as it grows.
//!
//! Usage:
//! ```text
//! dadhichi                     # run the built-in demo goal
//! dadhichi "your goal here"    # run the agent against your own goal
//! ```

mod approve;
mod cli;
mod console;
mod skill;
mod vault;

use std::sync::Arc;

use dadhichi_agent::{
    Agent, AgentContext, ConversationalAgent, MemoryRecallTool, MemoryWriteTool, Orchestrator,
    ReactAgent, SemanticMemory, SpecialistAgent, Workflow, shared_memory,
};
use dadhichi_ai::{MockEmbedder, ProviderPlan};
use dadhichi_cache::RocksBlobCache;
use dadhichi_collab::Rga;
use dadhichi_core::Kernel;
use dadhichi_index::store::SqliteSymbolStore;
use dadhichi_index::{Indexer, store::SymbolStore};
use dadhichi_mcp::{
    ApprovalPolicy, EchoTool, FsListTool, FsReadTool, FsWriteTool, GrantSet, Permission,
    PermissionMode, StateStore, TerminalTool, ToolRegistry, WorkspaceStore,
};
use dadhichi_skill::{Skill, SkillAgent, SkillRegistry, SkillStep};
use dadhichi_telemetry::Metrics;
use dadhichi_wasm::WasmRuntime;
use dadhichi_workspace::Workspace;

#[tokio::main]
async fn main() {
    // Fast-path flags before booting anything: packaging tools and installers
    // invoke `--version`/`--help` and expect an instant, side-effect-free
    // response on stdout with a zero exit code.
    let goal_override = match cli::parse(std::env::args().skip(1)) {
        cli::Command::Version => {
            println!("{}", cli::version_line());
            return;
        }
        cli::Command::Help => {
            println!("{}", cli::help_text());
            return;
        }
        cli::Command::Vault(cmd) => {
            vault::run(cmd);
            return;
        }
        cli::Command::Skill(cmd) => {
            skill::run(cmd);
            return;
        }
        cli::Command::Run { goal } => goal,
    };

    init_tracing();

    // 1. Boot the microkernel.
    let kernel = Kernel::new();
    println!("dadhichi ▸ kernel booted");

    // 2. Register core services into the kernel's service registry.
    //
    // Providers are resolved from the environment: set `ANTHROPIC_API_KEY`,
    // `OPENAI_API_KEY` (with optional `OPENAI_BASE_URL`), `OPENROUTER_API_KEY`,
    // or `OLLAMA_HOST` to use a real model; `DADHICHI_PROVIDER` picks the
    // default when several are set. With none set, the offline mock provider is
    // used so the whole run stays local and needs no keys.
    let plan = ProviderPlan::from_env();
    println!("dadhichi ▸ model provider: {}", plan.summary());
    let model_id = plan.default_model();
    let router = Arc::new(plan.build_router());

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());

    // The tool registry the agent actually acts through. The shell is pinned to
    // the workspace root, and the virtual filesystem is confined to it by a
    // path-jail, so nothing the agent does can escape the project directory.
    let fs_store: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&cwd));
    let cli_memory = shared_memory();
    let tools = {
        let t = ToolRegistry::new();
        t.register(Arc::new(EchoTool));
        t.register(Arc::new(TerminalTool::in_dir(&cwd)));
        t.register(Arc::new(FsReadTool::new(fs_store.clone())));
        t.register(Arc::new(FsWriteTool::new(fs_store.clone())));
        t.register(Arc::new(FsListTool::new(fs_store.clone())));
        t.register(Arc::new(MemoryWriteTool::new(cli_memory.clone())));
        t.register(Arc::new(MemoryRecallTool::new(cli_memory.clone())));
        Arc::new(t)
    };

    let mut workspace = Workspace::new();
    workspace.add_root(cwd.clone());

    kernel.services().register(router.clone()).await;
    kernel.services().register(tools.clone()).await;
    kernel.services().register(Arc::new(workspace)).await;
    println!(
        "dadhichi ▸ registered {} core services",
        kernel.services().len().await
    );

    // 3. Attach the Agent Console to the event bus.
    let console = console::spawn(kernel.bus());

    // 3a. When the user gave a concrete goal, run the tool-using ReAct agent so
    //     it can actually *do* the task — gated by a stdin approval prompt for
    //     shell commands and file writes — then exit. This short-circuits the
    //     workspace-indexing and capability showcase below, which only run for
    //     the bare `dadhichi` invocation, keeping a real goal run fast and quiet.
    if let Some(goal) = goal_override {
        println!("dadhichi ▸ goal: {goal}\n");

        // Gate the consequential capabilities behind the terminal approver, then
        // grant them: each shell command / file write pauses for a y/N before it
        // runs. Read-only tool calls proceed silently.
        tools.set_policy(
            ApprovalPolicy::default()
                .with(Permission::RunCommands, PermissionMode::Interrupt)
                .with(Permission::WriteWorkspace, PermissionMode::Interrupt),
        );
        tools.set_approver(Arc::new(approve::CliApprover));

        let mut ctx = AgentContext::new(
            router.clone(),
            tools.clone(),
            GrantSet::from_iter([
                Permission::ReadWorkspace,
                Permission::WriteWorkspace,
                Permission::RunCommands,
            ]),
            kernel.bus().clone(),
        );

        let agent = ReactAgent::new(&model_id);
        match agent.run(&goal, &mut ctx).await {
            Ok(outcome) => {
                println!(
                    "\ndadhichi ▸ {} finished ({:?})",
                    agent.name(),
                    outcome.status
                );
                println!("dadhichi ▸ answer: {}", outcome.summary);
            }
            Err(err) => eprintln!("\ndadhichi ▸ agent failed: {err}"),
        }

        // Drain the console and exit — skip the capability showcase.
        drop(ctx);
        drop(kernel);
        let _ = console.await;
        return;
    }

    // 3b. Index the workspace: tree-sitter parse → SQLite store, incremental
    //     and event-emitting, with parse results memoised in a persistent
    //     RocksDB blob cache. This is the Phase 2 code-intelligence pipeline.
    let store = Arc::new(SqliteSymbolStore::in_memory().expect("open symbol store"));
    let mut indexer = Indexer::new(store.clone()).with_event_bus(kernel.bus().clone());
    let cache_dir = std::env::temp_dir().join("dadhichi-parse-cache");
    match RocksBlobCache::open(&cache_dir) {
        Ok(cache) => indexer = indexer.with_blob_cache(Arc::new(cache)),
        Err(err) => eprintln!("dadhichi ▸ blob cache unavailable ({err}); parsing uncached"),
    }
    match indexer.index_dir(std::env::current_dir().unwrap_or_else(|_| ".".into())) {
        Ok(total) => {
            println!("dadhichi ▸ indexed {total} symbols across the workspace");
            // Go-to-definition lookup against the symbol index.
            if let Ok(defs) = store.definitions("main")
                && let Some(def) = defs.first()
            {
                println!(
                    "dadhichi ▸ 'main' defined at {}:{}",
                    def.file.display(),
                    def.line
                );
            }
            // Call-graph query: who calls `index_source`?
            if let Ok(callers) = store.callers_of("index_source") {
                println!(
                    "dadhichi ▸ 'index_source' has {} call site(s) in the codebase",
                    callers.len()
                );
            }
        }
        Err(err) => eprintln!("dadhichi ▸ indexing failed: {err}"),
    }

    // 3c. Semantic memory: embed notes and recall by meaning (vector search).
    let mut semantic = SemanticMemory::new(Arc::new(MockEmbedder::default()));
    for note in [
        "Dadhichi is agent-native",
        "The kernel is a microkernel",
        "Agents use MCP tools",
    ] {
        let _ = semantic.remember(note, serde_json::json!({})).await;
    }
    if let Ok(hits) = semantic.recall("The kernel is a microkernel", 1).await
        && let Some(hit) = hits.first()
    {
        println!(
            "dadhichi ▸ semantic recall top hit: {:?} (score {:.2})",
            hit.payload["text"], hit.score
        );
    }

    // The bare-invocation demonstration goal.
    let goal = "Explain what makes Dadhichi an agent-native IDE.".to_string();
    println!("dadhichi ▸ goal: {goal}\n");

    let mut ctx = AgentContext::new(
        router.clone(),
        tools.clone(),
        GrantSet::from_iter([Permission::ReadWorkspace]),
        kernel.bus().clone(),
    );

    let agent = ConversationalAgent::new(&model_id);
    match agent.run(&goal, &mut ctx).await {
        Ok(outcome) => {
            println!(
                "\ndadhichi ▸ {} finished ({:?})",
                agent.name(),
                outcome.status
            );
            println!("dadhichi ▸ confidence: {:.0}%", outcome.confidence * 100.0);
            println!("dadhichi ▸ answer: {}", outcome.summary);
        }
        Err(err) => {
            eprintln!("\ndadhichi ▸ agent failed: {err}");
        }
    }

    // 4b. Phase 4 — multi-agent workflow automation from natural language.
    //     The request is decomposed and each clause delegated to a specialist.
    let mut orchestrator = Orchestrator::new();
    for specialist in [
        SpecialistAgent::code(),
        SpecialistAgent::refactor(),
        SpecialistAgent::test(),
        SpecialistAgent::docs(),
        SpecialistAgent::review(),
        SpecialistAgent::git(),
        SpecialistAgent::security(),
    ] {
        orchestrator.register(Arc::new(specialist.with_model(&model_id)));
    }

    let request = "Refactor authentication, write tests, and update the documentation";
    println!("\ndadhichi ▸ workflow: {request}");
    let workflow = Workflow::parse(request);
    let report = workflow.execute(&orchestrator, &ctx).await;
    for step in &report.steps {
        println!(
            "dadhichi ▸   {} → {} ({:.0}%)",
            step.agent,
            if step.ok { "ok" } else { "failed" },
            step.confidence * 100.0
        );
    }
    println!(
        "dadhichi ▸ workflow confidence: {:.0}%",
        report.overall_confidence() * 100.0
    );

    // 4c. Phase 5 — extensibility, collaboration, security, observability.
    println!("\ndadhichi ▸ phase 5 capabilities:");

    // Sandboxed WASM plugin: run untrusted code under fuel metering.
    if let Ok(wasm) = wat::parse_str(
        r#"(module (func (export "add") (param i32 i32) (result i32)
             local.get 0 local.get 1 i32.add))"#,
    ) && let Ok(mut plugin) = WasmRuntime::new().instantiate(&wasm, 1_000_000)
        && let Ok(sum) = plugin.call_ii_i("add", 40, 2)
    {
        println!("dadhichi ▸   wasm plugin add(40,2) = {sum} (sandboxed, fuel-metered)");
    }

    // CRDT collaboration: two replicas converge under concurrent edits.
    let mut a = Rga::new(1);
    let mut b = Rga::new(2);
    let ops: Vec<_> = "hi"
        .chars()
        .enumerate()
        .map(|(i, c)| a.insert(i, c))
        .collect();
    for op in ops {
        b.apply(op);
    }
    let oa = a.insert(2, '!');
    let ob = b.insert(0, '>');
    b.apply(oa);
    a.apply(ob);
    println!(
        "dadhichi ▸   crdt converged: replica-a={:?} replica-b={:?} (equal={})",
        a.text(),
        b.text(),
        a.text() == b.text()
    );

    // Security: catch a leaked credential before it is committed.
    let sample = "let token = \"ghp_0123456789abcdefghij\";";
    let findings = dadhichi_security::scan(sample);
    println!(
        "dadhichi ▸   secret scan flagged {} credential(s)",
        findings.len()
    );

    // Observability: metrics captured this session.
    let metrics = Metrics::new();
    metrics.incr("agent.runs", 1 + report.steps.len() as u64);
    metrics.incr("symbols.indexed", store.symbol_count().unwrap_or(0) as u64);
    println!("dadhichi ▸   metrics: {}", metrics.snapshot());

    // 4d. Skills — reusable, permission-scoped capability bundles an agent
    //     equips. A skill bounds what tools a run can reach, independently of
    //     the grants it carries.
    println!("\ndadhichi ▸ skills:");
    // Built-ins plus any user/project manifests on disk (~/.dadhichi/skills,
    // ./.dadhichi/skills, $DADHICHI_SKILLS_DIR).
    let (skills, skill_load) = SkillRegistry::discover();
    println!(
        "dadhichi ▸   {} skills available: {}",
        skills.len(),
        skills.names().join(", ")
    );
    if !skill_load.loaded.is_empty() {
        println!(
            "dadhichi ▸   loaded {} skill(s) from disk: {}",
            skill_load.loaded.len(),
            skill_load.loaded.join(", ")
        );
    }
    for err in &skill_load.errors {
        eprintln!("dadhichi ▸   skipped malformed skill manifest: {err}");
    }

    // Equip a skill that is scoped to the `echo` tool and invokes it, then
    // asks the model to summarise — all through the same permission gate.
    let echo_skill = Skill::new("echo-demo", "Demonstrate a scoped tool call")
        .with_instructions("Summarise the tool output for the user.")
        .allow_tools(["echo"])
        .step(SkillStep::tool(
            "echo the greeting",
            "echo",
            serde_json::json!({ "value": "hello from a skill" }),
        ));
    let skill_agent = SkillAgent::new(echo_skill).with_model(&model_id);
    let mut skill_ctx = AgentContext::new(
        router.clone(),
        tools.clone(),
        GrantSet::from_iter([Permission::ReadWorkspace]),
        kernel.bus().clone(),
    );
    match skill_agent.run("Greet the user", &mut skill_ctx).await {
        Ok(outcome) => println!(
            "dadhichi ▸   skill '{}' → {:?} ({:.0}%)",
            skill_agent.skill().name,
            outcome.status,
            outcome.confidence * 100.0
        ),
        Err(err) => eprintln!("dadhichi ▸   skill run failed: {err}"),
    }

    // The capability contract is enforced: a skill that requires WriteWorkspace
    // is refused when the run was only granted ReadWorkspace.
    let write_skill = dadhichi_skill::builtin::implement();
    let denied = SkillAgent::new(write_skill)
        .run("edit a file", &mut skill_ctx)
        .await;
    println!(
        "dadhichi ▸   write-skill under read-only grant: {}",
        if denied.is_err() {
            "refused (permission enforced)"
        } else {
            "allowed"
        }
    );
    drop(skill_ctx);

    // 5. Drain the console by dropping the kernel's bus handles.
    drop(ctx);
    drop(kernel);
    let _ = console.await;
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}
