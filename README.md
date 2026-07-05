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
SQLite indexing pipeline with a call graph and RocksDB parse cache; an LSP
client; and vector/semantic search. Phase 3 (The Shell) — **complete**: a
toolkit-agnostic UI core (editor, explorer, problems, command palette, agent
console) rendered by a real ratatui terminal frontend, plus integrated
pseudo-terminal and Git view-models. Phase 4 (Autonomous Agents) —
**complete**: specialist agents, an orchestrator with parallel runs and
checkpoints, natural-language workflow automation, reflection/verification, a
live stdio MCP client with tool bridging, and a DAP debugger client. Phase 5
(Extensibility & Collaboration) — **complete**: a sandboxed WASM plugin runtime,
a signed extension marketplace, CRDT collaboration, a security suite (vault,
secret scan, audit log, injection defense), and observability. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the phased plan toward the full
autonomous agentic IDE, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for
the complete system design.

## Install

One-line installers detect your OS/architecture, download the matching release
archive, verify its SHA-256 checksum, and install the `dadhichi` binary:

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.ps1 | iex
```

Or install via Homebrew:

```bash
brew tap pariharshyamu/dadhichi https://github.com/pariharshyamu/Dadhichi
brew install dadhichi          # or: brew install --HEAD dadhichi
```

Native packages (`.deb`, `.rpm`, macOS `.app`, Windows `.msi`) and per-platform
tarballs are attached to each [GitHub Release](https://github.com/pariharshyamu/Dadhichi/releases).
See [`docs/PACKAGING.md`](docs/PACKAGING.md) for the full packaging story.

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
dadhichi ▸ model provider: offline (mock provider only)
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

## Use a real model

Dadhichi is model-agnostic. To drive a real provider, just set an environment
variable before running — the runtime detects it, registers the provider, and
keeps the offline mock as an automatic fallback:

```bash
# Anthropic
ANTHROPIC_API_KEY=sk-ant-...        cargo run -- "Refactor this module"

# OpenAI (or any OpenAI-compatible endpoint via OPENAI_BASE_URL)
OPENAI_API_KEY=sk-...               cargo run
OPENAI_API_KEY=... OPENAI_BASE_URL=http://localhost:8000/v1  cargo run   # vLLM, LM Studio, Azure…

# OpenRouter
OPENROUTER_API_KEY=sk-or-...        cargo run

# Local Ollama (no key)
OLLAMA_HOST=http://localhost:11434  cargo run
```

| Variable | Effect |
| --- | --- |
| `ANTHROPIC_API_KEY` | Use the Anthropic Messages API |
| `OPENAI_API_KEY` | Use OpenAI (`OPENAI_BASE_URL` overrides the endpoint) |
| `OPENROUTER_API_KEY` | Use the OpenRouter aggregator |
| `OLLAMA_HOST` | Use a local Ollama server (no key) |
| `DADHICHI_PROVIDER` | Pick the default when several are set (`anthropic`/`openai`/`openrouter`/`ollama`/`mock`) |

At startup the binary prints which provider is active, e.g.
`dadhichi ▸ model provider: anthropic (default: anthropic, + mock fallback)`.
Keys are read only from the environment — they are never logged (the provider
plan's `Debug` redacts them) and never written to disk. With no variable set,
the run stays fully offline on the mock provider.

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
├── dadhichi-dap         # DAP debugger client: breakpoints, threads, events
├── dadhichi-cache       # persistent RocksDB blob cache for parse results
├── dadhichi-vector      # vector store + cosine-kNN semantic search
├── dadhichi-ui          # toolkit-agnostic UI core: panels, editor, palette
├── dadhichi-term        # integrated pseudo-terminal sessions (portable-pty)
├── dadhichi-git         # Git view-model (git2): branch, status, commit, log
├── dadhichi-tui         # terminal frontend (ratatui) rendering the UI core
├── dadhichi-collab      # CRDT collaboration: convergent text, cursors, presence
├── dadhichi-security    # credential vault, secret scan, audit log, injection guard
├── dadhichi-telemetry   # metrics registry + crash reporting
├── dadhichi-wasm        # sandboxed WASM plugin runtime (wasmi, fuel-metered)
├── dadhichi-app         # application controller: wires kernel + agents + UI live
├── dadhichi-plugin      # plugin SDK: manifest, capabilities, signed marketplace
└── dadhichi             # runtime binary: wires the kernel + Agent Console
```

Launch the live terminal shell with `cargo run -p dadhichi-tui` — Ctrl-P opens
the command palette, which dispatches real kernel commands (run an agent,
re-index the workspace) whose progress streams into the panels. Render a
headless snapshot with `cargo run -p dadhichi-tui --example live` (the wired
stack) or `--example snapshot` (the static layout).

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
