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

## Phase 2 — Real Models & Editing

- [ ] Concrete providers behind `LanguageModel`: Anthropic, OpenAI, Ollama,
      OpenRouter (reqwest, streaming SSE)
- [ ] Prompt/context caching; capability-aware routing with fallback & cost
      accounting
- [ ] Tree-sitter parsing feeding `SymbolIndex`; `notify` file watcher →
      incremental re-index
- [ ] SQLite persistence for the symbol/reference/call graph
- [ ] LSP client service (go-to-def, hover, diagnostics, references)

## Phase 3 — The GUI Shell

- [ ] GPU-accelerated shell (GPUI/Slint + wgpu), < 20 ms interaction latency
- [ ] Editor (rope + incremental layout), Explorer, Problems, Timeline
- [ ] Chat panel + Agent Console as event-bus-driven panels
- [ ] Command Palette over the `CommandRegistry`; universal + semantic search
- [ ] Integrated GPU terminal (portable-pty) and built-in Git UI (git2-rs)

## Phase 4 — Autonomous Agents & MCP at Scale

- [ ] Specialised agents: code, refactor, test, review, docs, security, git
- [ ] Multi-agent delegation, checkpoints/rollback, background & parallel runs
- [ ] Reflection/verification passes with confidence scoring
- [ ] LanceDB long-term memory with embedding recall; summarisation & pruning
- [ ] Live MCP clients (stdio/WebSocket) for GitHub, Docker, Kubernetes, DBs
- [ ] DAP debugger service; workflow automation from natural language

## Phase 5 — Extensibility, Collaboration & Reach

- [ ] `wasmtime`/WASI plugin runtime + signed extension marketplace
- [ ] Live collaboration (CRDT) with shared cursors, presence, AI participant
- [ ] Full security suite: WASI sandbox, secret vault, SAST/DAST, audit log,
      prompt-injection defenses
- [ ] OpenTelemetry tracing/metrics; crash reporting; opt-in telemetry
- [ ] Cross-platform packaging (Linux/macOS/Windows); WASM edition; mobile
      companion

## Guiding invariants (all phases)

- No subsystem depends on another directly — only events, commands, services.
- Every new backend is a trait implementation, never a call-site change.
- Offline-first: the system must remain usable and testable with no network.
- Secure by design: capabilities are granted explicitly and enforced at one
  choke point per boundary.
