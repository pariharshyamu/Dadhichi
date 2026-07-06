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
live stdio MCP client with tool bridging, and a DAP debugger client. Agents can
**delegate** self-contained sub-tasks to a specialist via the `task` tool (or
the `agent.spawn` command), which runs it in an **isolated context window** —
only the task goes in and only the summary comes back — so a delegate's
intermediate reasoning never pollutes the caller's context. Phase 5
(Extensibility & Collaboration) — **complete**: a sandboxed WASM plugin runtime,
a signed extension marketplace, CRDT collaboration, a security suite (vault,
secret scan, audit log, injection defense), and observability. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the phased plan toward the full
autonomous agentic IDE, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for
the complete system design.

## Install

One-line installers detect your OS/architecture, download the matching release
archive, verify its SHA-256 checksum, and install two binaries — the `dadhichi`
CLI and the interactive terminal shell `dadhichi-tui` (Ctrl-P for the command
palette; `>` runs skills, `@` manages MCP servers):

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

# Local Ollama (no key) — name the model you have pulled
OLLAMA_HOST=http://localhost:11434  OLLAMA_MODEL=llama3.2  cargo run
```

| Variable | Effect |
| --- | --- |
| `ANTHROPIC_API_KEY` | Use the Anthropic Messages API |
| `OPENAI_API_KEY` | Use OpenAI (`OPENAI_BASE_URL` overrides the endpoint) |
| `OPENROUTER_API_KEY` | Use the OpenRouter aggregator |
| `OLLAMA_HOST` | Use a local Ollama server (no key) |
| `OLLAMA_MODEL` | The Ollama model name to run (e.g. `llama3.2`) |
| `DADHICHI_PROVIDER` | Pick the default when several are set (`anthropic`/`openai`/`openrouter`/`ollama`/`mock`) |
| `DADHICHI_MODEL` | The concrete model name sent to the default provider (e.g. `gpt-4o`, `llama3.2`) |

> **Model name vs. provider:** the provider id (`ollama`, `openai`, …) selects the
> backend; the *model name* is what it runs. Real backends reject a request for a
> model literally named `ollama`, so set `OLLAMA_MODEL`/`DADHICHI_MODEL` to a
> concrete model. Without one, the name defaults to the provider id (fine only for
> the offline mock).

At startup the binary prints which provider is active, e.g.
`dadhichi ▸ model provider: anthropic (default: anthropic, + mock fallback)`.
Keys are read only from the environment — they are never logged (the provider
plan's `Debug` redacts them) and never written to disk. With no variable set,
the run stays fully offline on the mock provider.

## Skills

A **skill** is a reusable, permission-scoped capability bundle an agent equips —
not a single tool, but a recipe that combines an instruction prompt, the
permissions a run must hold, an allow-list of tools it may reach, and a plan
template. Crucially, a skill's tool scope is *narrower than* the run's grants:
even holding `WriteWorkspace`, a skill scoped to `["fs.read"]` cannot touch
`fs.write`. Tool access is the intersection of the run's grants and the skill's
allow-list, enforced at one choke point (`ScopedTools`).

```rust
use dadhichi_skill::{Skill, SkillAgent, SkillRegistry, Permission};

let review = Skill::new("code-review", "Review a change")
    .with_instructions("You are a meticulous reviewer.")
    .require(Permission::ReadWorkspace)
    .allow_tools(["fs.read", "git.diff"]);   // read-only, no matter the grants

let mut skills = SkillRegistry::with_builtins();   // explain, code-review, implement, …
skills.register(review);

let agent = SkillAgent::new(skills.get("code-review").unwrap());
// `agent` implements the same Agent trait, so the orchestrator runs it like any other.
```

Skills are plain data (`Serialize`/`Deserialize`), so besides authoring them in
code you can drop **JSON manifests** on disk and they're loaded automatically:

```
~/.dadhichi/skills/*.json        # your personal skills
<workspace>/.dadhichi/skills/*.json   # project skills, checked into the repo
$DADHICHI_SKILLS_DIR/*.json      # an explicit override (highest precedence)
```

A disk skill overrides a built-in of the same name, so you can customise a
shipped skill by name. A manifest looks like:

```json
{
  "name": "house-style",
  "description": "Apply our house style",
  "instructions": "Follow the team style guide; prefer clarity over cleverness.",
  "required_permissions": ["read_workspace", "write_workspace"],
  "tools": { "mode": "allow", "names": ["fs.read", "fs.write"] },
  "steps": [{ "description": "read the file" }, { "description": "apply the change" }]
}
```

Malformed manifests are reported (as a `skill.load.error` in the console), never
fatal. See the live demonstration, which also loads a skill from disk:

```bash
cargo run -p dadhichi-skill --example run_skill
```

which equips a pure-prompt skill, a tool-scoped skill (invoking `echo` through
the gate), and shows a write-scoped skill **refused** under a read-only grant.
In the IDE, the command palette's `>` skill mode is the picker — it lists the
skills with their permissions and tool scope inline (backed by `skill.list`) and
runs the chosen one. Under the hood, `skill.run` equips a skill granted exactly
the permissions it declares, and `skill.reload` re-scans the manifest
directories and swaps the catalogue live. The manifest directories are also
**watched**: editing or dropping a `*.json` skill file reloads the catalogue
automatically and refreshes the `>` picker, no command or restart needed.

## MCP connectors

Dadhichi is MCP-native. Point it at any MCP server — GitHub, Slack, Figma, a
filesystem, Postgres — by declaring it in an `mcp.json` (in `~/.dadhichi/` or
`<workspace>/.dadhichi/`). At boot the servers are launched, their tools are
discovered, namespaced (`github.create_issue`), stamped with the permissions you
declare, and registered — so an agent invokes a remote tool exactly like a
built-in one, through the same permission gate.

A server is either a **local subprocess** (`command`) or a **hosted endpoint**
(`url`) — Dadhichi picks the transport from the URL scheme: `http(s)://` for
Streamable HTTP (JSON or SSE, with the `Mcp-Session-Id` carried across calls),
`ws(s)://` for WebSocket. Hosted servers take `headers` (e.g. an `Authorization`
bearer token), which support the same `${...}` secret placeholders as `env`.

```json
{
  "servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${env:GITHUB_TOKEN}" },
      "grants": ["network"]
    },
    "linear": {
      "url": "https://mcp.linear.app/mcp",
      "headers": { "Authorization": "Bearer ${env:LINEAR_TOKEN}" },
      "grants": ["network"]
    }
  }
}
```

Secrets never live in the config. A `${...}` placeholder resolves either as
`env:NAME` (from the environment) or `vault:NAME` (from an encrypted
ChaCha20-Poly1305 credential vault), so tokens need never sit in plaintext env
vars. Populate the vault out-of-process — the running IDE only reads it:

```sh
export DADHICHI_VAULT_PASSPHRASE='…'          # unlocks ~/.dadhichi/vault.json
printf %s "$GITHUB_TOKEN" | dadhichi vault set github_token
dadhichi vault list                            # names only; values stay encrypted
```

```json
"env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${vault:github_token}" }
```

An unresolved secret refuses that server rather than launching it blank. If a
server fails to start it's reported (`mcp.error`) but never fatal. The networked
transports live behind the crate's `remote` feature (enabled in the app binary);
a pure-offline build keeps only the stdio path.

Manage connections at runtime. `mcp.list` shows every configured server with its
transport, connected state, and tool count; `mcp.connect { server? }` connects one
or all; `mcp.disconnect { server }` drops a connection and unregisters exactly its
tools. The `@` palette mode browses the servers and toggles each — connecting a
disconnected one, disconnecting a connected one — and refreshes live.

Tools aren't the only capability. A server's readable **resources** and prompt
**templates** are bridged too: `mcp.resources` / `mcp.resource.read` pull in
context, and `mcp.prompts` / `mcp.prompt.get` instantiate server-authored prompts —
so an agent can read a server's data and reuse its prompts, not just call its tools.

## Workspace layout

```
crates/
├── dadhichi-core        # microkernel: event bus, command layer, service registry
├── dadhichi-ai          # model-agnostic AI runtime: LanguageModel trait, router,
│                        #   OpenAI/Anthropic providers, fallback + cost accounting
├── dadhichi-mcp         # MCP layer: tools, permission-gated registry, JSON-RPC
├── dadhichi-agent       # agent framework: planning, memory, lifecycle, tools
├── dadhichi-skill       # skills: permission-scoped capability bundles agents equip
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

Launch the live terminal shell with `cargo run -p dadhichi-tui` (or the
installed `dadhichi-tui`). It opens focused on the **Agent Console**: type a
goal on the input line at the bottom and press **Enter** to run the
conversational agent against it — its planning/running/token/completion events
stream into the console above as it works. The run is **non-blocking**: the
console keeps updating (and shows a `⋯ running` indicator) while the model
thinks, so a slow local model never freezes the UI. **Ctrl-P** opens the command
palette, which dispatches real kernel commands (run a specific agent, re-index
the workspace) whose progress streams into the panels. Type `>` in the palette to
switch to **skill mode**: it lists the equippable skills with their required
permissions and tool scope inline, and Enter runs the highlighted one; `@`
switches to **MCP mode**, which lists your configured servers (Enter toggles
connect/disconnect) **and** the built-in connector catalogue — pick one (e.g.
`filesystem`, `github`, `git`, `memory`) and Enter adds it to your `mcp.json` and
connects it, no hand-editing. **Tab** cycles focus between panes, **Ctrl-Q**
quits.

Install your own **skills** by dropping a JSON manifest into `~/.dadhichi/skills`
(the running TUI reloads it live), or with the CLI:

```bash
dadhichi skill import ./my-skill.json   # validate + install into ~/.dadhichi/skills
dadhichi skill list                     # built-ins + everything on disk
```
Render a headless snapshot with `cargo run -p dadhichi-tui --example live` (the
wired stack) or `--example snapshot` (the static layout).

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
