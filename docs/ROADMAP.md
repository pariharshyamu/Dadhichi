# Dadhichi — Phased Roadmap

From a minimal viable kernel to a full autonomous agentic development
environment. Each phase is shippable and builds strictly on the previous one.

## Phase 1 — Minimum Viable Kernel ✅ (this repository)

The bootable foundation, fully offline and tested.

- [x] Microkernel: event bus, command layer, service registry (`dadhichi-core`)
- [x] Model-agnostic AI runtime: `LanguageModel` trait, router, streaming,
      offline `MockProvider` (`dadhichi-ai`)
- [x] MCP/tool layer: `Tool`, permission-gated `ToolRegistry`, JSON-RPC envelope
      (`dadhichi-mcp`)
- [x] Agent framework: plan/step decomposition, tiered memory, lifecycle,
      reference agent (`dadhichi-agent`)
- [x] Workspace model + incremental symbol index (`dadhichi-workspace`)
- [x] Plugin SDK: manifest, capability gating, host lifecycle (`dadhichi-plugin`)
- [x] Runtime binary wiring the kernel + Agent Console (`dadhichi`)
- [x] 18 unit tests + doctests, clippy-clean, rustfmt-enforced

## Phase 2 — Real Models & Editing ✅ (complete)

- [x] Concrete providers behind `LanguageModel`: `OpenAiProvider` (OpenAI,
      OpenRouter, Ollama, vLLM, LM Studio via the shared `/chat/completions`
      schema) and `AnthropicProvider`, both with reqwest + streaming SSE
      (`dadhichi-ai`, `http` feature)
- [x] Capability-aware routing with fallback chain (`complete_resilient`) and
      cost accounting (`CostTable`/`ModelPricing`)
- [x] Prompt/context caching: provider-side cache breakpoints (`Message::cached`
      → Anthropic `cache_control`) and a client-side response cache
      (`CachingModel`/`CompletionCache`)
- [x] Tree-sitter parsing extracting symbols (`dadhichi-parse`, Rust grammar)
- [x] `notify` file watcher → incremental re-index emitting `symbols.updated`
      (`dadhichi-index`)
- [x] SQLite persistence for symbols (`SqliteSymbolStore`); reference/call graph
      still to come
- [x] LSP client service — hover, go-to-definition, references, and live
      diagnostics over stdio, with responses correlated by id and diagnostics
      republished as `lsp.diagnostics` events (`dadhichi-lsp`)
- [x] Reference/call-graph tables: the parser extracts call/use edges attributed
      to their enclosing function; the SQLite store answers `callers_of` /
      `callees_of` (`dadhichi-parse`, `dadhichi-index`)
- [x] RocksDB blob cache memoising parse results by content hash, so unchanged
      files are never re-parsed (`dadhichi-cache`)
- [x] Vector store + semantic search: cosine-kNN `VectorStore`, an
      `EmbeddingModel` abstraction (mock + OpenAI), and `SemanticMemory` for
      embed-and-recall (`dadhichi-vector`, `dadhichi-ai`, `dadhichi-agent`). The
      in-memory store is the reference backend; LanceDB is the drop-in
      production backend behind the same trait.

## Phase 3 — The Shell ✅ (complete)

- [x] Toolkit-agnostic UI application core (`dadhichi-ui`): the `App` aggregate
      with one-way data flow (`apply_event`), reused by any renderer
- [x] Rope-backed editor (`ropey`) with incremental edits and cursor navigation
- [x] Explorer (collapsible file tree), Problems (fed by `lsp.diagnostics`),
      and Chat / Agent Console as event-bus-driven panels
- [x] Command Palette over the command names, with fuzzy ranking
- [x] A real, running terminal frontend (`dadhichi-tui`, ratatui + crossterm)
      that renders the shared view-models; verified headlessly against a
      `TestBackend`
- [x] Integrated pseudo-terminal sessions (`dadhichi-term`, portable-pty)
- [x] Built-in Git view-model (`dadhichi-git`, git2): branch, status, stage,
      commit, history

The GUI shell is a ratatui TUI rather than a `wgpu`/GPUI window: a GPU surface
can neither run nor be tested in a headless CI environment. Because rendering
draws purely from `dadhichi-ui`'s view-models and holds no state, a GPU shell
plugs in by writing a new `render` over the same `App` — no application logic
changes. `< 20 ms` latency and the GPU pipeline remain a GPU-shell concern.

## Phase 4 — Autonomous Agents & MCP at Scale ✅ (complete)

- [x] Specialised agents: `SpecialistAgent` with code / refactor / test / review
      / docs / git / security constructors, each planning and reflecting
- [x] Orchestrator: run by name, parallel fan-out on forked contexts, and
      `Checkpoint` capture/restore for rollback
- [x] Reflection/verification: `HeuristicVerifier` scores confidence from real
      structural signals (plan completion, answer substance)
- [x] Memory summarisation & pruning (`Memory::prune`,
      `summarise_conversation`); semantic recall shipped in Phase 2
- [x] Live MCP client: transport-generic `McpConnection` (line-delimited
      JSON-RPC, id-correlated) with `connect_stdio`, plus `McpToolBridge` that
      exposes discovered external tools through the permission-gated registry
- [x] DAP debugger service (`dadhichi-dap`): initialize, breakpoints, threads,
      continue, with adapter events republished as `dap.<event>`
- [x] Workflow automation from natural language: `Workflow::parse` decomposes a
      request and delegates each clause to the right specialist
- [x] Skills (`dadhichi-skill`): reusable, permission-scoped capability bundles
      (instructions + required grants + a tool allow-list + a plan template).
      `ScopedTools` enforces that a skill's reachable tools are the intersection
      of the run's grants and the skill's allow-list; `SkillAgent` runs one as a
      first-class agent. A built-in library plus a filesystem loader
      (`~/.dadhichi/skills`, `<workspace>/.dadhichi/skills`) ships, wired into
      the binary and `AppController` via `skill.list` / `skill.run` /
      `skill.reload` (hot-reload without a restart)

Deferred to a later pass: a WebSocket MCP transport, and the LanceDB backend for
durable long-term memory (the `VectorStore` trait already abstracts it).

## Phase 5 — Extensibility, Collaboration & Reach ✅ (complete)

- [x] Sandboxed WASM plugin runtime (`dadhichi-wasm`, wasmi): fuel metering, a
      host capability boundary, and memory isolation
- [x] Signed extension marketplace (`dadhichi-plugin::marketplace`): Ed25519
      publisher signatures + SHA-256 module integrity, verified before load
- [x] Live collaboration (`dadhichi-collab`): an RGA text CRDT that converges
      under concurrent edits, plus shared cursors/presence with an AI-participant
      flag
- [x] Security suite (`dadhichi-security`): ChaCha20-Poly1305 credential vault,
      secret detection, a hash-chained tamper-evident audit log, and
      prompt-injection assessment + quarantine
- [x] Observability (`dadhichi-telemetry`): a metrics registry (counters/gauges)
      with JSON snapshot, and a crash reporter with consent-gated upload
- [x] Cross-platform packaging: a tag-triggered matrix release
      (`.github/workflows/release.yml`) builds binaries for Linux/macOS/Windows
      (x86_64 + aarch64), archives each with a SHA-256 checksum, and builds
      `.deb`/`.rpm`/`.app`/`.msi` plus a self-updating Homebrew formula;
      checksum-verifying one-line installers (`packaging/install.{sh,ps1}`) and
      CI-validated packaging assets (`packaging/verify.sh`) round it out. See
      [`PACKAGING.md`](PACKAGING.md).
- [ ] WASM edition (wasm32) and mobile companion — the remaining reach work

The plugin runtime uses `wasmi` (a portable interpreter) rather than
`wasmtime`/WASI for a fast, dependency-light build; the JIT + full WASI slot in
behind the same `WasmRuntime` surface. SAST/DAST are the remaining security
additions on top of the shipped suite.

## Guiding invariants (all phases)

- No subsystem depends on another directly — only events, commands, services.
- Every new backend is a trait implementation, never a call-site change.
- Offline-first: the system must remain usable and testable with no network.
- Secure by design: capabilities are granted explicitly and enforced at one
  choke point per boundary.
