# Dadhichi — System Architecture

This document is the architectural reference for Dadhichi, an AI-first,
agent-native IDE written in Rust. It maps directly onto the 24 deliverables of
the design brief. Sections marked **[implemented]** correspond to code that
exists and runs in this repository today; sections marked **[design]** describe
the target architecture the current foundation is built to grow into.

---

## 1. System Architecture — the microkernel

Dadhichi uses a **microkernel** architecture. The kernel
(`dadhichi-core::Kernel`) owns exactly three cross-cutting primitives and
nothing domain-specific:

| Primitive | Purpose | Style |
|-----------|---------|-------|
| `EventBus` | broadcast notifications between subsystems | fire-and-forget fan-out |
| `CommandRegistry` | named, awaitable request/response intents | typed result |
| `ServiceRegistry` | shared, long-lived capabilities resolved by type | dependency injection |

Every capability — AI runtime, agents, workspace/index, LSP, DAP, terminal,
git, plugins — is a **service** that plugs into these seams. **No subsystem
depends on another directly.** A subsystem publishes events, dispatches
commands, and resolves the services it needs by type. This decoupling is what
makes the whole system modular, testable, and hot-pluggable at runtime.

```
                 ┌───────────────────────────────┐
   UI shell ───▶ │           Kernel              │
 (Slint/GPUI)    │  EventBus │ Commands │ Services│
                 └───┬────────────┬──────────┬────┘
       publish/sub   │  dispatch  │  resolve  │
        ┌────────────┴──┬─────────┴──┬────────┴─────────┐
        ▼               ▼            ▼                  ▼
   AI Runtime      Agent Fwk    Workspace/Index    Plugin Host
   (dadhichi-ai) (dadhichi-agent)(dadhichi-workspace)(dadhichi-plugin)
        │               │            │                  │
        └──── MCP / Tool layer (dadhichi-mcp) ──────────┘
                        │
              External APIs · LSP · DAP · Git · Terminal · FS
```

**[implemented]** The kernel, all three primitives, and the service wiring in
`crates/dadhichi/src/main.rs`.

---

## 2. Cargo Workspace Layout

A Cargo workspace (`resolver = "3"`, edition 2024) with one crate per bounded
context. Dependencies flow **downward only** — leaf crates never depend on the
binary, and horizontal dependencies between core crates are avoided.

```
crates/
├── dadhichi-core        # kernel primitives — depends on nothing internal
├── dadhichi-ai          # AI runtime      — depends on nothing internal
├── dadhichi-mcp         # tools + MCP     — depends on nothing internal
├── dadhichi-workspace   # index model     — depends on nothing internal
├── dadhichi-parse       # tree-sitter     — depends on workspace
├── dadhichi-cache       # RocksDB cache   — depends on nothing internal
├── dadhichi-vector      # vector store    — depends on nothing internal
├── dadhichi-index       # indexing svc    — depends on core, workspace, parse, cache
├── dadhichi-lsp         # LSP client      — depends on core
├── dadhichi-dap         # DAP client      — depends on core
├── dadhichi-ui          # UI core         — depends on core
├── dadhichi-term        # terminal        — depends on nothing internal
├── dadhichi-git         # git view-model  — depends on nothing internal
├── dadhichi-tui         # TUI frontend    — depends on ui, git
├── dadhichi-agent       # agents          — depends on core, ai, mcp, vector
├── dadhichi-plugin      # plugin SDK      — depends on core
└── dadhichi             # binary          — depends on all of the above
```

Shared dependency versions and lints are centralised in
`[workspace.dependencies]` and `[workspace.lints]`.

**[implemented]** All seven crates build, test, and lint clean.

---

## 3. Module Dependency Graph

```
             dadhichi-core ◀────────────┐◀───────────┐◀──────────┐
                  ▲                      │            │           │
                  │                 dadhichi-agent    │           │
             dadhichi-plugin             ▲            │           │
                  ▲             ┌────────┴────────┐   │           │
                  │             │                 │   │           │
                  │        dadhichi-ai      dadhichi-mcp          │
                  │             ▲                 ▲               │
                  │             │                 │               │
                  └──────────── dadhichi (binary) ────── dadhichi-workspace
```

The graph is acyclic. `dadhichi-agent` is the only crate that composes several
others, because agents are the point where models (ai), tools (mcp), and the
kernel (core) meet.

**[implemented]** Enforced by the crate manifests.

---

## 4. Data Flow

Two canonical flows exercise the whole system:

**Agent run (implemented):**
```
user goal ─▶ Agent::run ─▶ Plan (decompose)
                          ─▶ emit agent.status/plan on EventBus ─▶ Agent Console
                          ─▶ ModelRouter::complete ─▶ LanguageModel
                          ─▶ ToolRegistry::invoke (permission-checked)
                          ─▶ Memory::remember
                          ─▶ AgentOutcome (status, summary, confidence, plan)
```

**File edit → re-index (design, index implemented):**
```
notify watcher ─▶ fs.changed event ─▶ Indexer service
              ─▶ tree-sitter parse ─▶ SymbolIndex::index_file (incremental)
              ─▶ symbols.updated event ─▶ code-intel consumers
```

---

## 5. Event Bus Design

Built on `tokio::sync::broadcast`: one sender, many receivers, non-blocking
publish. Design choices:

- **Topics** namespace events (`"agent.status"`, `"fs.changed"`) so subscribers
  filter cheaply without deserializing every payload.
- **Opaque JSON payloads** (`serde_json::Value`) let any subsystem — including
  dynamically loaded WASM plugins — emit and consume events with no
  compile-time coupling to the emitter's types.
- **Correlation ids** thread related events together (e.g. every event from one
  agent run), which is how the Agent Console groups activity.
- **Back-pressure is decoupled from correctness**: a slow subscriber observes
  `RecvError::Lagged(n)` rather than stalling the publisher, then resynchronises.

**[implemented]** `dadhichi-core::bus`.

---

## 6. AI Runtime Abstraction

The single `LanguageModel` trait fronts every backend — OpenAI, Anthropic,
Gemini, DeepSeek, Qwen, Mistral, Llama, Ollama, LM Studio, vLLM, OpenRouter,
Azure, Vertex, and local runtimes (llama.cpp, ONNX, Candle). The rest of the
IDE only ever sees this trait, which is what makes Dadhichi **model-agnostic**.

- `complete()` returns a whole response; `stream()` returns a `BoxStream` of
  deltas and **defaults to wrapping `complete()`**, so a provider without native
  streaming still satisfies the contract.
- `ModelCapabilities` (vision, reasoning, tools, embeddings, local) lets the
  `ModelRouter` match a model to a task.
- `ModelRouter` is the seam for **routing, fallback, load balancing, and cost
  optimisation**: today it routes by explicit id or capability and falls back to
  a default; that policy grows without touching call sites.
- `MockProvider` is a deterministic, offline provider used for tests, demos, and
  offline-first operation.

Caching operates at two layers: **provider-side prompt caching**
(`Message::cached()` inserts an Anthropic `cache_control` breakpoint over a
stable prefix) and **client-side response caching** (`CachingModel` wraps any
provider and serves byte-identical repeat requests from a `CompletionCache`,
skipping the network entirely).

**[implemented]** `dadhichi-ai`, including concrete `OpenAiProvider` (OpenAI /
OpenRouter / Ollama / vLLM / LM Studio) and `AnthropicProvider` over reqwest
with SSE streaming, the `complete_resilient` fallback chain, a `CostTable` for
per-completion pricing, both caching layers, and an `EmbeddingModel`
abstraction (`MockEmbedder` + `OpenAiEmbedder`) feeding semantic search.
**[design]** rerankers.

---

## 7. Agent Framework

An `Agent` pursues a natural-language goal through a **plan → act → reflect**
loop. Every specialised agent (code, refactor, test, review, docs, architecture,
security, git, debug, database, infra, CI/CD, knowledge) is just another
implementation of the one `Agent` trait, so the orchestrator treats them
uniformly and new agent types need no runtime changes.

- `Plan` / `Step` — task decomposition with per-step completion and a `progress()`
  metric.
- `AgentContext` — bundles shared services (model router, tool registry, event
  bus) with per-run state (memory, permission grants, correlation id) and an
  `emit()` helper for progress events.
- `AgentStatus` — `Idle → Planning → Running → (Paused) → Completed | Cancelled
  | Failed`, surfaced live to the Agent Console.
- `AgentOutcome` — terminal status, summary, self-assessed confidence, and the
  final audited plan.
- `ConversationalAgent` — the reference implementation exercising the full loop
  offline.

**[implemented]** `dadhichi-agent`, including seven specialist agents
(`SpecialistAgent`: code, refactor, test, review, docs, git, security); an
`Orchestrator` that runs agents by name, fans several out **in parallel** on
forked contexts, and captures/restores `Checkpoint`s for rollback;
reflection/verification via `HeuristicVerifier` (confidence from real structural
signals); and `Workflow`, which decomposes a natural-language request and
**delegates** each clause to the right specialist. **[design]** richer
per-step (rather than one-shot) execution and model-driven planning.

---

## 8. Plugin SDK

Third-party extensions ship as **sandboxed WASM modules** (WASI). The host-side
contract is a declarative `Manifest` plus a `Plugin` lifecycle trait
(`activate`/`deactivate` for hot-reload). A plugin declares the `Capability`
set it needs up front; `PluginHost::load` **refuses activation unless every
requested capability was granted**, so a plugin can only ever reach what its
manifest declared and the user approved. The trait is the stable ABI boundary
the WASM runtime marshals across.

**[implemented]** `dadhichi-plugin` (native in-process plugins + capability
gating). **[design]** the `wasmtime`/WASI runtime, marketplace, versioning,
dependency isolation.

---

## 9. MCP Implementation

Dadhichi is **MCP-native**: both a client (consuming external servers — GitHub,
Docker, Kubernetes, databases, Slack, Linear, Jira, cloud providers) and a
server (exposing its own workspace tools to other agents).

- `Tool` — the unit of capability: JSON-in/JSON-out with a self-describing
  `ToolSpec` (name, description, JSON-Schema, required permissions).
- `ToolRegistry` — catalogues tools and is the **single choke point** where
  permissions are enforced against a `GrantSet`; nothing reaches a tool without
  passing the gate.
- `protocol` — JSON-RPC 2.0 envelope plus `McpClient` trait and capability
  negotiation for bridging external MCP servers behind the same `Tool` trait.

**[implemented]** `dadhichi-mcp`, now including a live `McpConnection` — a
transport-generic, id-correlated JSON-RPC client with a `connect_stdio`
constructor — and `McpToolBridge`, which discovers a remote server's tools and
exposes each through the permission-gated `ToolRegistry` (defaulting external
tools to the `Network` scope). An agent invokes a GitHub or Docker MCP tool
exactly as it invokes a built-in one. **[design]** a WebSocket transport and
auth.

---

## 10. UI Architecture

The UI is split into a **toolkit-agnostic core** and a **renderer**, so the
application logic is written and tested once and every frontend reuses it.

- **`dadhichi-ui`** owns the view-models — a rope-backed `Document` editor, the
  `Explorer` tree, the `ProblemsPanel`, the fuzzy `CommandPalette`, and the
  chat/agent transcript — aggregated in `App`. It owns *no business logic and no
  rendering*. `App::apply_event` is the single seam where bus events mutate UI
  state, realising the one-way flow **UI intent → command → service → event →
  view-model update**.
- **`dadhichi-tui`** is a concrete renderer (ratatui + crossterm) that draws
  `App` and translates keystrokes into view-model calls. It holds no state, so a
  `wgpu`/GPUI shell plugs in by writing a new `render` over the same `App`.

**[implemented]** the UI core and the terminal frontend, both unit-tested —
the TUI renders against ratatui's `TestBackend` so the full multi-panel layout
is verified headlessly. **[design]** the GPU shell (GPUI/Slint + `wgpu`) and its
`< 20 ms` frame budget; because the render thread only reads view-models and all
I/O is async on Tokio, that target is a renderer concern, not an architectural
one.

---

## 11. Database Schema **[design]**

Three stores, each chosen for its access pattern:

- **SQLite** — structured, queryable metadata: symbols, references, the
  call/dependency graph, settings, command history. Schema sketch:
  `files(id, path, root_id, mtime, hash)`,
  `symbols(id, file_id, name, kind, line, col)`,
  `refs(symbol_id, file_id, line)`, `edges(from_symbol, to_symbol, kind)`.
- **RocksDB** — high-throughput cache: parsed ASTs, incremental build artifacts.
- **Vector store** — embeddings powering semantic search and long-term agent
  memory.

**[implemented]** the SQLite `SqliteSymbolStore` now holds both the `symbols`
table and a `refs` table (the call/reference graph), answering `definitions`,
`callers_of`, and `callees_of`. `dadhichi-cache::RocksBlobCache` is the working
RocksDB blob cache — the indexer memoises each file's parse result by content
hash, so an unchanged file is never re-parsed. `dadhichi-vector` provides the
`VectorStore` trait with an exact cosine-kNN `InMemoryVectorStore`; **[design]**
LanceDB is the drop-in on-disk ANN backend behind the same trait, and the
`files`/`edges`-with-kinds schema refinement.

---

## 12. Memory System

Layered, tiered memory for agents:

| Tier | Lifetime | Backend (target) |
|------|----------|------------------|
| `Working` | current step | in-memory, cleared per step |
| `Conversation` | current session | in-memory / SQLite |
| `LongTerm` | durable | vector store + SQLite |

Two retrieval paths exist: `Memory::recall()` for keyword recall, and
`SemanticMemory` for **recall by meaning** — it embeds each item with an
`EmbeddingModel` and stores it in a `VectorStore`, so a query retrieves the
nearest items by cosine similarity even when the wording differs.
`Memory::prune` bounds working memory (evicting oldest, never `LongTerm`) and
`summarise_conversation` collapses the transcript into a durable summary.
**[design]** the LanceDB backend for durable, scalable semantic memory.

**[implemented]** `dadhichi-agent::memory` and `dadhichi-agent::semantic`, with
`MockEmbedder` for offline determinism and `OpenAiEmbedder` for real embeddings.

---

## 13. IPC Protocol **[design]**

The GUI process and the kernel communicate over **JSON-RPC** framed on a local
socket (or in-process channel when co-located). The same `Command`/`Event`
model serializes directly: a command dispatch is a request, an event is a
server-push notification. This is deliberately the same shape as MCP, so the
transport layer is shared between "the UI talking to the kernel" and "the kernel
talking to an MCP server".

---

## 14. RPC Interfaces

- **Internal**: `CommandRegistry::dispatch(Command) -> Result<Value>` is the
  universal RPC surface between UI and services.
- **External MCP**: `McpClient::{initialize, call}` over the JSON-RPC envelope
  in `dadhichi-mcp::protocol`.
- **LSP**: `dadhichi-lsp::LspClient` speaks the Language Server Protocol over
  stdio — Content-Length framing, id-correlated requests, and notifications
  (diagnostics) forwarded onto the event bus as `lsp.diagnostics`.
- **DAP**: `dadhichi-dap::DapClient` — the same Content-Length framing and
  id-correlation as LSP, with adapter events (`stopped`, `terminated`) forwarded
  onto the bus as `dap.<event>`.

**[implemented]** internal command RPC, MCP envelope + live client, and the LSP
and DAP clients.

---

## 15. State Management

State ownership is strict and singular:

- The **workspace/index** owns file and symbol state; mutated only through the
  indexer, exposed read-only elsewhere.
- Each **agent run** owns its plan and memory via `AgentContext`.
- Shared services live in the `ServiceRegistry` behind `Arc`; interior
  mutability uses `tokio::sync::RwLock`, never blocking locks on the async
  runtime.
- The UI holds **no authoritative state** — it is a projection of events.

**[implemented]** across core, agent, workspace.

---

## 16. Rendering Pipeline **[design]**

`wgpu`-backed GPU rendering with a retained scene graph. Editor text uses a rope
data structure with incremental re-layout; only dirty regions re-render. The
minimap, syntax highlighting (tree-sitter), and diagnostics are separate layers
composited on the GPU. Frame budget: 16.6 ms (60 fps) with headroom for the
< 20 ms interaction target.

---

## 17. Thread Model

- A single **Tokio multi-threaded runtime** hosts all async work (I/O, model
  calls, agents, MCP). `async` everywhere; no blocking calls on runtime threads.
- CPU-bound work (parsing, indexing, search) runs on `spawn_blocking` /
  dedicated rayon pools **[design]** so it never starves the async executor.
- The GUI render thread **[design]** is isolated and communicates with the
  kernel only via channels — it never blocks on I/O.
- Shared state uses `Arc` + async `RwLock`; the event bus is lock-free on the
  publish path.

**[implemented]** the async runtime and shared-state discipline.

---

## 18. Security Model

Defense in depth, **secure by design**:

- **Capability-based tools**: every tool declares required `Permission`s; the
  `ToolRegistry` is the single enforcement point and denies calls whose
  `GrantSet` is insufficient.
- **Capability-based plugins**: `PluginHost` refuses to activate a plugin whose
  manifest requests an ungranted `Capability`.
- **Agent sandboxing**: agents act only through granted tools — they hold no
  ambient authority over the filesystem or network.
- **[design]**: WASI sandbox for plugins, secret detection and a credential
  vault, dependency/license scanning (SAST/DAST), prompt-injection guards on
  untrusted content, and an append-only audit log of every tool invocation.

**[implemented]** the two capability-enforcement choke points.

---

## 19. Performance Strategy

- **Incremental everything**: `SymbolIndex::index_file` replaces one file's
  symbols without touching the graph; tree-sitter re-parses only edited ranges.
- **Async I/O** so latency-bound work never blocks compute.
- **Zero-copy / cheap clones**: kernel handles are `Arc`-backed; `Topic`
  filtering avoids payload deserialization.
- **[design]**: GPU acceleration, background indexing, memory pools, SIMD search
  (ripgrep), lazy loading, prompt/context caching to cut model cost and latency.

---

## 20. Testing Strategy

- **Unit tests** in every crate (18 today) cover the event bus, command
  dispatch, service resolution, routing, streaming, permission gating, the
  agent loop, incremental indexing, and plugin capability enforcement.
- **Doctests** double as living usage examples on each crate's public API.
- **Offline determinism**: `MockProvider` makes the entire agent pipeline
  reproducible with no network, so CI needs no secrets.
- **[design]**: property tests (proptest), integration tests across the kernel,
  benchmarks (criterion), mutation and snapshot testing.

**[implemented]** unit + doctests, all green.

---

## 21. Deployment Pipeline **[design]**

`cargo build --release` (thin LTO, single codegen unit, `panic = "abort"`)
produces a static-ish binary per platform. CI matrix on GitHub Actions:
fmt → clippy (deny warnings) → test → build across Windows/Linux/macOS →
package → sign → release. A `SessionStart` hook keeps web sessions able to run
fmt/clippy/test.

---

## 22. Cross-Platform Packaging **[design]**

- **Linux**: AppImage + `.deb`/`.rpm`.
- **macOS**: signed & notarised `.app` / `.dmg` (universal arm64 + x86_64).
- **Windows**: MSI/MSIX, signed.
- **Future**: WebAssembly build of the core for an in-browser edition; a mobile
  companion consuming the same JSON-RPC surface.

Because the kernel is pure Rust with no platform-specific logic, only the UI
shell and packaging differ per OS.

---

## 23. Extension Marketplace **[design]**

A registry of signed WASM plugins keyed by `Manifest` (id, version,
capabilities). Install resolves and verifies signatures, presents the requested
capabilities for user consent, and sandboxes the module under WASI. Versioning
follows semver against the stable plugin ABI; dependency isolation prevents one
plugin's crate graph from affecting another.

---

## 24. Future Roadmap

See [`ROADMAP.md`](ROADMAP.md) for the phased plan. In brief: **Phase 1**
(this repo) — bootable microkernel + AI runtime + agent framework + MCP/tool
layer + plugin SDK, all offline and tested. Later phases add real model
providers, the GUI shell, LSP/DAP/terminal/git services, the WASM plugin
runtime, collaboration (CRDT), and the full autonomous multi-agent
orchestration described in the brief.

---

## Idiomatic Rust commitments

Trait-based abstractions at every seam · event-driven decoupling · `Arc` +
async `RwLock` for shared state · `unsafe_code = "warn"` at the workspace level
(zero `unsafe` in the current code) · errors via `thiserror` with a typed
`Result` per crate · every public item carries Rustdoc · `clippy::all` clean ·
`cargo fmt` enforced.
