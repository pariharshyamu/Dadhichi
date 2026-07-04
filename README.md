# Dadhichi

**A modern, AI-first, agent-native IDE written in Rust.**

Dadhichi is not another code editor with a chat sidebar. It is a development
environment designed from the kernel up around autonomous AI agents working
alongside humans — model-agnostic, offline-first, MCP-native, and extensible
through sandboxed WASM plugins.

This repository contains the **foundational runtime**: a working microkernel
with an event bus, command layer, a model-agnostic AI runtime, a permission-gated
tool/MCP layer, an autonomous agent framework, an incremental workspace index,
and a plugin SDK — all wired into a bootable headless binary that plans and
executes an agent run end to end.

## Status

Phase 1 (Minimum Viable Kernel) — **complete**. Phase 2 (Real Models &
Editing) — **complete**: real OpenAI/Anthropic providers with SSE streaming,
fallback routing, cost accounting, and prompt/response caching; a tree-sitter →
SQLite indexing pipeline driven by a file watcher; and an LSP client for hover,
go-to-definition, references, and live diagnostics. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the phased plan toward the full
autonomous agentic IDE, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for
the complete system design.

## Quick start

```bash
# Build everything
cargo build

# Run the whole test suite (unit + doctests)
cargo test

# Boot the kernel and run the built-in demo agent
cargo run

# Run the agent against your own goal
cargo run -- "Explain what makes Dadhichi agent-native."
```

Running `cargo run` boots the microkernel, registers the core services,
attaches the Agent Console to the event bus, and drives a `ConversationalAgent`
through a plan → act → reflect loop against the offline mock model provider:

```
dadhichi ▸ kernel booted
dadhichi ▸ registered 3 core services
dadhichi ▸ goal: Explain what makes Dadhichi an agent-native IDE.

dadhichi ▸ conversational-agent finished (Completed)
dadhichi ▸ confidence: 90%
dadhichi ▸ answer: [dadhichi-mock] Explain what makes Dadhichi an agent-native IDE.
  ┃ [agent.status] status="planning"
  ┃ [agent.plan] steps=3
  ┃ [agent.status] status="running"
  ┃ [agent.tokens] total=42
  ┃ [agent.status] confidence=0.9 status="completed"
```

Everything runs **offline** — the default `MockProvider` needs no network — so
the whole system is reproducible and testable without API keys.

## Workspace layout

```
crates/
├── dadhichi-core        # microkernel: event bus, command layer, service registry
├── dadhichi-ai          # model-agnostic AI runtime: LanguageModel trait, router,
│                        #   OpenAI/Anthropic providers, fallback + cost accounting
├── dadhichi-mcp         # MCP layer: tools, permission-gated registry, JSON-RPC
├── dadhichi-agent       # agent framework: planning, memory, lifecycle, tools
├── dadhichi-workspace   # workspace model + incremental symbol index
├── dadhichi-parse       # tree-sitter symbol + call-graph extraction (Rust)
├── dadhichi-index       # indexing service: file watcher + SQLite + blob cache
├── dadhichi-lsp         # LSP client: hover, definition, references, diagnostics
├── dadhichi-cache       # persistent RocksDB blob cache for parse results
├── dadhichi-vector      # vector store + cosine-kNN semantic search
├── dadhichi-plugin      # plugin SDK: manifest, capabilities, host lifecycle
└── dadhichi             # runtime binary: wires the kernel + Agent Console
```

Each crate has its own crate-level Rustdoc (`cargo doc --open`) and unit tests.

## Design principles

AI-first · agent-native · offline-first · model-agnostic · language-agnostic ·
MCP-native · GPU-accelerated (roadmap) · cross-platform · extensible ·
secure by design.

The architecture is a **microkernel**: no subsystem depends on another
directly. They communicate through the event bus (fire-and-forget) and the
command layer (request/response), and resolve shared capabilities from the
service registry. This is what makes every subsystem — including the AI runtime
and third-party plugins — hot-pluggable.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full treatment.

## License

Apache-2.0.
