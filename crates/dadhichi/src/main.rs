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

mod console;

use std::sync::Arc;

use dadhichi_agent::{Agent, AgentContext, ConversationalAgent, SemanticMemory};
use dadhichi_ai::{MockEmbedder, MockProvider, ModelRouter};
use dadhichi_cache::RocksBlobCache;
use dadhichi_core::Kernel;
use dadhichi_index::store::SqliteSymbolStore;
use dadhichi_index::{Indexer, store::SymbolStore};
use dadhichi_mcp::{EchoTool, GrantSet, Permission, ToolRegistry};
use dadhichi_workspace::Workspace;

#[tokio::main]
async fn main() {
    init_tracing();

    // 1. Boot the microkernel.
    let kernel = Kernel::new();
    println!("dadhichi ▸ kernel booted");

    // 2. Register core services into the kernel's service registry.
    let router = {
        let mut r = ModelRouter::new();
        // Offline-first: the mock provider needs no network. Real providers
        // (Anthropic, OpenAI, Ollama, …) register here behind the same trait.
        r.register(Arc::new(MockProvider::default()));
        Arc::new(r)
    };

    let tools = {
        let mut t = ToolRegistry::new();
        t.register(Arc::new(EchoTool));
        Arc::new(t)
    };

    let mut workspace = Workspace::new();
    workspace.add_root(std::env::current_dir().unwrap_or_else(|_| ".".into()));

    kernel.services().register(router.clone()).await;
    kernel.services().register(tools.clone()).await;
    kernel.services().register(Arc::new(workspace)).await;
    println!(
        "dadhichi ▸ registered {} core services",
        kernel.services().len().await
    );

    // 3. Attach the Agent Console to the event bus.
    let console = console::spawn(kernel.bus());

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

    // 4. Run an agent.
    let goal = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Explain what makes Dadhichi an agent-native IDE.".to_string());
    println!("dadhichi ▸ goal: {goal}\n");

    let mut ctx = AgentContext::new(
        router,
        tools,
        GrantSet::from_iter([Permission::ReadWorkspace]),
        kernel.bus().clone(),
    );

    let agent = ConversationalAgent::default();
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
