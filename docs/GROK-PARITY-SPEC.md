# Grok Build Parity — Implementation Spec

Status: **proposed** · Target baseline: **v0.1.13** · Scope: adopt the
best-practice patterns from SpaceXAI's open-source [`xai-org/grok-build`](https://github.com/xai-org/grok-build)
(a Rust terminal coding agent) into Dadhichi, grounded in Dadhichi's current
crates.

This document is an *implementation* spec, not a roadmap: every phase gives
concrete types, the exact insertion point in the existing code, a migration /
back-compat story, a test plan, an effort size, and the risks. Phases are
ordered by dependency — **P0 → P1 → P2**.

Legend for effort: **S** ≈ ≤1 day, **M** ≈ 2–4 days, **L** ≈ ≥1 week.

---

## 0. Where Dadhichi stands today (ground truth)

The permission model we build on lives in `dadhichi-mcp`:

- `Permission` (`crates/dadhichi-mcp/src/tool.rs:36`) is **coarse** — four
  variants: `ReadWorkspace`, `WriteWorkspace`, `RunCommands`, `Network`.
- `GrantSet` (`registry.rs:10`) is the *static* gate: does the caller hold the
  capability at all.
- `ApprovalPolicy` (`approval.rs:35`) maps each `Permission` → `PermissionMode`
  (`Allow` / `Interrupt` / `Deny`); `decide(&[Permission])` returns the worst
  (`Deny > Interrupt > Allow`).
- `ToolRegistry::invoke` (`registry.rs:136`) is the **single choke point**: it
  checks `grants.first_missing`, then `policy.decide`, then the async `Approver`.
- The call `args` JSON already carries the content we need to match on
  (`terminal.run` → `args["command"]`; `fs.*` → `args["path"]`).

The gap vs grok-build: **Dadhichi decides on the capability class, grok decides
on the content** (this command string, this path glob, this `server__tool`),
merged from layered config with `deny > ask > allow` severity. Everything in P0
closes that gap without breaking the existing choke point.

Other ground-truth anchors used below:
- `dadhichi-security` exports `audit` (hash-chained `AuditLog`), `injection`,
  `secrets`, `vault`, `resolver` — the natural home for the OS sandbox and
  folder-trust.
- `dadhichi-core::Kernel` = `EventBus` + `CommandRegistry` + `ServiceRegistry`;
  there is **no config loader yet**.
- `dadhichi-git::GitRepo` (libgit2) has `open/init/status/stage_all/commit/log`
  — no worktree ops yet.
- `dadhichi-agent::Delegator::delegate(agent, spec, task, base, cwd)` +
  `DelegationReview::land()`; `SubAgentSpec::{for_role, roster, read_only,
  writer, with_skills}`.
- `dadhichi/src/session.rs` persists a capped `Vec<MemoryItem>` blob to
  `.dadhichi/session.json` — no resume/rewind/fork.

---

# Phase P0 — Safety & trust foundation

Prerequisite for every later phase. Three deliverables: a content-aware
permission-rule engine, named permission modes, and a folder-trust store.

## P0.1 — Permission-rule engine  ·  effort: **L**

### New module: `crates/dadhichi-mcp/src/permission/`

```rust
// permission/rule.rs

/// The verdict a rule carries. Mirrors grok's deny > ask > allow and maps 1:1
/// onto the existing PermissionMode (Deny/Interrupt/Allow), so the resolver can
/// return a PermissionMode and the registry is unchanged downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction { Allow, Ask, Deny }

/// The tool families a rule can target. Derived from the tool *name* at match
/// time (see `ToolClass::of`), not stored on the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass { Bash, Read, Edit, Grep, Mcp, WebFetch, WebSearch, Any }

impl ToolClass {
    /// Classify a registered tool by its registry name.
    /// terminal.run → Bash; fs.read → Read; fs.write → Edit;
    /// fs.grep/fs.glob → Grep; names containing "__" (server__tool) → Mcp; …
    pub fn of(tool_name: &str) -> ToolClass { /* … */ }
}

/// One rule: an action, the class it applies to, and an optional content
/// pattern. `pattern == None` matches every call of that class.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub action: RuleAction,
    pub tool: ToolClass,
    pub pattern: Option<Pattern>,
}

/// A prefix-or-glob matcher over the call's content string. Uses `globset`
/// (already a common dep) for glob; a plain prefix branch for the fast path.
#[derive(Debug, Clone)]
pub enum Pattern { Prefix(String), Glob(globset::GlobMatcher) }

impl PermissionRule {
    /// Parse the string form used in config and CLI flags, e.g.
    /// `Bash(git *)`, `Read(src/**)`, `MCPTool(linear__*)`, `WebFetch(domain:x.com)`.
    pub fn parse(action: RuleAction, s: &str) -> Result<Self, RuleParseError> { /* … */ }
}
```

```rust
// permission/mod.rs

/// The content a rule matches against, extracted from a live call.
pub struct ToolCall<'a> {
    pub name: &'a str,
    pub class: ToolClass,
    pub args: &'a serde_json::Value,
}

impl<'a> ToolCall<'a> {
    /// The string a Pattern matches: command for Bash, path for Read/Edit/Grep,
    /// url for WebFetch, the qualified name for Mcp.
    pub fn subject(&self) -> Option<Cow<'_, str>> { /* … */ }
    /// Shell-split segments for Bash (on && || ; | and newlines), with env-var
    /// prefixes and a fixed wrapper set (timeout/nice/env/…) peeled.
    pub fn bash_segments(&self) -> Vec<String> { /* … */ }
}

/// The merged rule set from every config source. Order does not matter —
/// evaluation is by severity, exactly as grok specifies.
#[derive(Debug, Clone, Default)]
pub struct RuleSet { rules: Vec<PermissionRule> }

impl RuleSet {
    pub fn push(&mut self, rule: PermissionRule) { self.rules.push(rule) }
    pub fn extend(&mut self, other: RuleSet) { self.rules.extend(other.rules) }

    /// Severity resolution: any matching Deny → Deny; else any matching Ask →
    /// Ask; else any matching Allow → Allow; else None (fall through to the
    /// built-in auto-approvals and then the mode policy).
    ///
    /// Deny/Ask are checked against *every* Bash segment and the whole string;
    /// Allow only against the whole command string (grok's asymmetry — narrow
    /// allow + explicit deny is the safe idiom).
    pub fn evaluate(&self, call: &ToolCall) -> Option<RuleAction> { /* … */ }
}
```

```rust
// permission/dangerous.rs  &  permission/readonly.rs

/// Word-boundary-matched commands that never auto-approve even under a
/// remembered grant: rm, chmod, chown, chgrp, chattr, pkill, kill, killall,
/// git push.
pub fn is_dangerous(primary_cmd: &str) -> bool { /* … */ }

/// Read-only shell primaries that auto-approve (ls, cat, pwd, head, tail, wc,
/// git status|log|diff|show, grep, rg, cargo check, …) — a *convenience*, never
/// a security boundary.
pub fn is_read_only_command(primary_cmd: &str) -> bool { /* … */ }
```

### The resolver (the new decision function)

```rust
// permission/resolver.rs

/// Combines, in grok's documented order:
///   1. RuleSet (deny > ask > allow, layered config)
///   2. remembered per-project grants  (P0.3 folder-trust store)
///   3. built-in auto-approvals (read-only tools + read-only shell)
///   4. the mode policy (ApprovalPolicy) — the existing behaviour
#[derive(Debug, Clone, Default)]
pub struct PermissionResolver {
    pub rules: RuleSet,
    pub mode: PermissionMode,        // the session default (see P0.2)
    pub remember: RememberedGrants,  // P0.3
}

impl PermissionResolver {
    /// Returns Allow / Interrupt / Deny for a concrete call. This is what the
    /// registry consults; `ApprovalPolicy::decide` becomes the final fallback
    /// inside this function.
    pub fn resolve(&self, call: &ToolCall, required: &[Permission],
                   policy: &ApprovalPolicy) -> PermissionMode { /* … */ }
}
```

### Insertion point (surgical, back-compat)

In `ToolRegistry::invoke` (`registry.rs:136`), replace the bare
`policy.decide(&spec.permissions)` (line 156) with a resolver call that also
sees the tool name + args:

```rust
let call = ToolCall { name, class: ToolClass::of(name), args: &args };
let verdict = self.resolver().resolve(&call, &spec.permissions, &self.policy());
match verdict { /* Allow | Deny | Interrupt — identical arms to today */ }
```

Add `resolver: RwLock<PermissionResolver>` beside the existing `policy` /
`approver` fields, plus `set_resolver` / `resolver()` accessors mirroring
`set_policy` / `policy`.

### Back-compat guarantee

- Default `PermissionResolver` = empty `RuleSet` + `PermissionMode::Allow` +
  empty remembers ⇒ `resolve()` falls straight through to
  `ApprovalPolicy::decide` ⇒ **byte-for-byte today's behaviour**. All existing
  `approval.rs`, `registry.rs`, and `shell.rs` tests pass untouched.
- `ApprovalPolicy` and `PermissionMode` are **kept**, not replaced — the rule
  engine layers *in front of* them.

### Test plan

- `rule::parse` round-trips each documented form incl. the `cmd:*` suffix and
  `domain:` WebFetch form.
- `RuleSet::evaluate`: deny beats allow regardless of insertion order and
  source; allow matches whole-string only; a denied Bash *segment* rejects a
  chained command (`git status && rm -rf /`).
- `is_read_only_command` word-boundary: `ls` ≠ `lsof`; `git status` ✓, `git push` ✗.
- Integration through `ToolRegistry::invoke`: a `Bash(rm -rf *)` deny rule
  rejects `terminal.run` even when `RunCommands` is granted and mode is Allow.
- Regression: the four existing approval/registry tests still pass with a
  default resolver installed.

### Risks

- **Shell parsing correctness** is the crux (command substitution, subshells).
  Mitigation: mirror grok — anything that can't be split into simple segments
  (`$(...)`, backticks, `&`, redirection) is treated as a single unit that
  *prompts*, never silently allowed.
- `globset` semantics differ subtly from grok's hand-rolled matcher (`**`
  crossing `/`). Document the exact subset supported; add explicit tests.

---

## P0.2 — Permission modes  ·  effort: **S**

Grok's `default` / `dontAsk` / `acceptEdits` / `bypassPermissions`. We already
have the runtime enum shape; this is naming + wiring, not new machinery.

```rust
// permission/mode.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    #[default] Default,        // prompt for anything not pre-approved
    DontAsk,                   // deny anything without an explicit allow/auto-approve
    AcceptEdits,               // auto-approve Edit-class, prompt the rest
    BypassPermissions,         // auto-approve, but deny rules + shell asks still apply
}
```

`PermissionResolver::resolve` consults `SessionMode` as step 4:
- `Default` → unmatched calls Interrupt (or Allow for read-only auto-approve).
- `DontAsk` → unmatched calls Deny (this is the correct **headless** default).
- `AcceptEdits` → `ToolClass::Edit` unmatched → Allow.
- `BypassPermissions` → unmatched → Allow, **but** a matching Deny rule and any
  Bash `Ask` rule still win (grok's short-circuit contract).

Wiring: `--permission-mode` CLI flag in `crates/dadhichi/src/cli.rs`; a
`/mode` TUI command; enterprise lock via a root-owned file check (see P0.3).

Tests: each mode's fall-through verdict for a read-only tool, an edit, a shell
command, and a deny-matched call.

---

## P0.3 — Folder-trust + config layering + remembered grants  ·  effort: **M**

### Config loader (new, in `dadhichi-core` or a small `dadhichi-config` crate)

There is no config loader today. Add a layered TOML loader that produces a
merged `RuleSet` + `SessionMode`, walking (lowest → highest precedence):

1. `~/.dadhichi/config.toml` (global)
2. every `<dir>/.dadhichi/config.toml` from repo root down to cwd (project)
3. `/etc/dadhichi/requirements.toml` (root-owned enterprise lock — can force
   `bypass` off; users cannot override)

`[permission]` accepts both the structured form
(`{ action = "deny", tool = "bash", pattern = "rm -rf *" }`) and the compact
string arrays (`deny = ["Bash(rm -rf *)"]`). Severity (`deny > ask > allow`) is
applied across **all** sources, so a global deny can't be undone by a project
allow — grok's key safety property.

### Folder-trust store

```rust
// dadhichi-security/src/trust.rs
/// One trust gate for hooks + repo-local MCP/LSP (P1 depends on this).
pub struct TrustStore { path: PathBuf /* ~/.dadhichi/trusted_folders.toml */ }
impl TrustStore {
    pub fn is_trusted(&self, dir: &Path) -> bool; // cascades to subdirectories
    pub fn trust(&self, dir: &Path) -> io::Result<()>;
    pub fn untrust(&self, dir: &Path) -> io::Result<()>;
}
```

Project-scoped `.dadhichi/config.toml` permission rules still load (they're
declarative and reviewable), but **project hooks/MCP** are gated on trust in P1.

### Remembered grants

Per-project "always allow `cargo test`" persisted outside the repo
(`~/.dadhichi/grants/<project-hash>.toml`); consulted as resolver step 2;
dangerous-list commands re-prompt regardless.

Tests: precedence (project deny overrides global allow → still deny; global deny
overrides project allow → deny); trust cascade to a subdir; a remembered grant
satisfies an Ask without re-prompting, but `git push` still prompts.

---

# Phase P1 — Extensibility & real isolation

Depends on P0 (trust store + resolver).

## P1.1 — Hooks (`dadhichi-hooks` crate)  ·  effort: **M**

Grok's cleanest, most copyable design. New crate `dadhichi-hooks`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart, UserPromptSubmit, PreToolUse /*blocking*/, PostToolUse,
    PostToolUseFailure, PermissionDenied, Stop, Notification,
    SubagentStart, SubagentStop, PreCompact, PostCompact, SessionEnd,
}

pub struct HookSpec {
    pub event: HookEvent,
    pub matcher: Option<regex::Regex>, // tests tool name (tool events only)
    pub kind: HookKind,                // Command{cmd,timeout,env} | Http{url,timeout}
}

pub struct HookRunner { specs: Vec<HookSpec>, trust: TrustStore }
impl HookRunner {
    /// Fire every matching hook. Only PreToolUse can block: an explicit
    /// {"decision":"deny","reason":…} on stdout (or exit 2) blocks the call.
    /// Everything else — crash, timeout, missing script — is FAIL-OPEN.
    pub async fn fire(&self, event: HookEvent, payload: &HookPayload) -> HookOutcome;
}
```

- **Discovery:** `~/.dadhichi/hooks/*.json` (always trusted) + project
  `.dadhichi/hooks/*.json` (**requires folder-trust** from P0.3).
- **Wiring:** `PreToolUse` fires inside `ToolRegistry::invoke` *before* the
  resolver (a hook deny stops the call; a hook allow falls through to the normal
  permission checks — matches grok). Passive events fire from the agent loop /
  bus. `SessionStart` fires from the binary boot; `Stop` from the agent turn end.
- **Payload** on stdin as JSON (`toolName`, `toolInput`, `cwd`, `sessionId`);
  runner-injected env (`DADHICHI_HOOK_EVENT`, `DADHICHI_SESSION_ID`,
  `DADHICHI_WORKSPACE_ROOT`).

Tests: a `PreToolUse` deny blocks a whitelisted-but-dangerous command; a crashing
hook fails open (call proceeds, failure surfaced on the bus); matcher regex
selects the right tool; untrusted project hook is silently skipped.

Risk: **fail-open is deliberate** — document loudly that hooks are automation,
not a security boundary (that's the sandbox, P1.2).

## P1.2 — OS-level sandbox (`dadhichi-security::sandbox`)  ·  effort: **L**

Today's `StateStore`/`PathJail` is application-level: a shelled `cat /etc/shadow`
via `terminal.run` escapes it. Close it with kernel enforcement, applied **once
at process start, irreversibly** (grok's model).

```rust
// dadhichi-security/src/sandbox.rs
pub enum SandboxProfile { Off, Workspace, ReadOnly, Strict, Custom(CustomProfile) }
pub struct CustomProfile {
    pub extends: Box<SandboxProfile>,
    pub restrict_network: bool,
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    pub deny: Vec<String>, // globs, kernel-enforced read+write/rename
}
pub fn apply(profile: &SandboxProfile) -> Result<(), SandboxError>;
```

- **Linux:** `landlock` crate for FS scoping + `seccompiler` for child-network
  block; a `deny` list needs bubblewrap-style bind-over (bind trick to also
  read-deny). **macOS:** Seatbelt (`sandbox_init` / a generated `.sb` profile).
- **Fail-closed for custom profiles** (unknown profile / bad glob / missing
  bubblewrap ⇒ refuse to start); **fail-open with a warning** for built-ins on
  an unsupported kernel (grok's exact asymmetry).
- Profile is fixed for a session's life and saved with it (ties into P2.6).
- Flag: `--sandbox <profile>`; config `[sandbox] profile`.

Tests (Linux CI): under `strict`, a child `cat` outside cwd is denied and a
child `curl` fails with `restrict_network`; a `deny = ["**/*.pem"]` glob blocks
both read and `mv secret x && cat x`. Gate macOS specifics behind
`#[cfg(target_os = "macos")]`.

Risks: **Landlock needs kernel ≥ 5.13** — the release aarch64 leg and older CI
images may lack it; detect and fail-open for built-ins, and add a capability
probe. This is the highest-complexity item in the whole spec — consider shipping
`workspace` + `read-only` first, `strict`/custom-deny second.

## P1.3 — Project rules (AGENTS.md)  ·  effort: **S–M**

```rust
// dadhichi-agent/src/rules.rs
pub struct ProjectRules { pub files: Vec<LoadedRule> }
pub struct LoadedRule { pub path: PathBuf, pub body: String, pub approx_tokens: usize }
impl ProjectRules {
    /// Walk repo-root → cwd; load AGENTS.md / AGENT.md / CLAUDE.md /
    /// CLAUDE.local.md + `.dadhichi/rules/*.md` at each level. Deeper files win
    /// (appended later). Skip .gitignore'd files.
    pub fn discover(cwd: &Path) -> ProjectRules;
    /// Concatenated, ready to fold into the system prompt.
    pub fn as_prompt_block(&self) -> String; // wrapped in <project_rules>…</project_rules>
}
```

Wire into `ReactAgent::system_prompt` (the persona/system-prompt builder already
exists per delegate work). Add `--rules "<text>"` (append) and a `dadhichi
inspect` subcommand listing discovered files + token counts (grok's `grok
inspect`).

Tests: three-level nesting accumulates and deeper wins on conflict; a
`.gitignore`'d `CLAUDE.local.md` is skipped; `--rules` text appears in the
prompt.

---

# Phase P2 — Workflow depth

## P2.1 — Worktree isolation for delegates  ·  effort: **M**

Extend `dadhichi-git`:

```rust
impl GitRepo {
    pub fn add_worktree(&self, name: &str, base: &str) -> Result<Worktree, GitError>;
    pub fn list_worktrees(&self) -> Result<Vec<Worktree>, GitError>;
    pub fn prune_worktree(&self, name: &str) -> Result<(), GitError>;
}
pub struct Worktree { pub path: PathBuf, pub branch: String }
```

Give the delegator an isolation choice alongside its existing `OverlayStore`:

```rust
// dadhichi-agent/src/delegate.rs
pub enum Isolation { Overlay, Worktree } // default Overlay (today's behaviour)
// Delegator::delegate gains isolation; Worktree runs the child in a real git
// worktree and DelegationReview::land() does a merge-back instead of overlay flush.
```

Overlay stays the zero-cost default; worktree is opt-in for file-mutating
delegates that must not collide with the parent. Tests: two worktree delegates
edit the same file without collision; `land()` merges a worktree branch back;
prune cleans up.

## P2.2 — Subagent capability modes + controls  ·  effort: **S–M**

`SubAgentSpec` already carries grants; add grok's coarse filter + guards:

```rust
pub enum CapabilityMode { ReadOnly, ReadWrite, Execute, All }
impl SubAgentSpec { pub fn with_capability(self, m: CapabilityMode) -> Self; }
```

`CapabilityMode` maps to the grant/tool-filter the delegator already builds
(read/ls always; write iff ReadWrite/All; terminal iff Execute/All). Add a
**depth limit = 1** guard (a delegate's registry omits the delegate/task tool)
and a `background` flag (spawn + return an id; result fetched later) reusing the
existing async agent-run plumbing. Tests: a ReadOnly delegate is denied
`fs.write`; a delegate cannot spawn a delegate (depth guard).

## P2.3 — Session lifecycle: resume / fork / rewind  ·  effort: **L**

Evolve `dadhichi/src/session.rs` from a single JSON blob into grok's model.

```rust
pub struct SessionStore { root: PathBuf } // ~/.dadhichi/sessions/<enc-cwd>/<id>/
// per session dir:
//   updates.jsonl    (append-only conversation + tool calls — source of truth)
//   summary.json     (id, title, model, timestamps, parent_session_id)
//   plan.json        (todo state)
//   rewind_points.jsonl  (file snapshots, one per user prompt)
impl SessionStore {
    pub fn new_session(&self, cwd: &Path) -> Session;        // UUIDv7 id
    pub fn resume(&self, id: &str) -> Result<Session, _>;    // grok --resume
    pub fn fork(&self, id: &str, directive: Option<String>) -> Session;
    pub fn rewind(&self, id: &str, point: usize) -> Result<(), _>; // restore file snapshots
    pub fn list(&self, cwd: &Path) -> Vec<SessionSummary>;
}
```

`rewind` alone is a large safety win (restores actual file snapshots, not a
model reconstruction). Keep the existing cross-run memory as the fast path;
sessions supersede it for the TUI. Optional: a SQLite FTS index for
`sessions search` (grok has this) — defer if `rusqlite` weight is unwanted
(`dadhichi-index`/`dadhichi-cache` may already pull a store).

Tests: resume replays `updates.jsonl` to the same state; fork gets a new id with
`parent_session_id` set; rewind to point N restores the snapshotted files and
truncates history.

## P2.4 — ACP + headless mode  ·  effort: **L**

Two independent deliverables; ship headless first (smaller, unblocks CI use).

- **Headless:** a `-p "<prompt>"` non-interactive path in `crates/dadhichi` that
  runs one agent turn and prints result; `--output-format json` emits
  `{sessionId, result, …}` so automations can capture the id and pass it to
  `--resume`. Under `-p`, a call that would prompt is **cancelled and reported to
  the model** (never hangs) — pair with `SessionMode::DontAsk`.
- **ACP:** a new `dadhichi-acp` crate implementing the Agent Client Protocol
  over stdio (`session/new`, `session/load`, streaming updates) so editors embed
  Dadhichi. Reuse the `SessionStore` from P2.3 as the persistence layer.

Tests: `-p` with a mock model returns a deterministic result and a resumable id;
ACP `session/new` → prompt → update stream round-trips against a test client.

---

# Cross-cutting principles to adopt wholesale

1. **Severity-based, layered config** — `deny` always wins across every source;
   evaluation is by severity, not order. (P0.1/P0.3)
2. **Fail-open hooks, fail-closed sandbox** — convenience automation must never
   brick a run; a security boundary that can't be applied must refuse to start.
   (P1.1 vs P1.2)
3. **Harness compatibility** — read `CLAUDE.md` / `.claude/settings.json` /
   `.cursor/` where cheap (P1.3, P0.3); meet users where they already are.
4. **One trust gate** — a single folder-trust store governs hooks + repo-local
   MCP/LSP together (P0.3 → P1.1).
5. **Read-only auto-approve is a convenience, not a boundary** — the boundary is
   grants + rules + sandbox. (P0.1)

---

# Sequencing & dependency graph

```
P0.1 rule engine ──┬─> P0.2 modes ──┐
                   │                 ├─> P1.1 hooks ──┐
P0.3 trust+config ─┘                 │               ├─> (feature-complete gate)
                                     │  P1.2 sandbox ─┘
                                     └─> P1.3 project-rules
P2.1 worktree ─> P2.2 capability-modes
P2.3 sessions ─> P2.4 ACP/headless
```

- **P0** is the critical path: nothing in P1 is safe or well-defined without the
  resolver + trust store.
- P1.1/P1.2/P1.3 are mutually independent once P0 lands (parallelizable).
- P2 tracks are independent of P1 and of each other; schedule by appetite.

## Effort roll-up

| Phase | Item | Effort |
|---|---|---|
| P0.1 | Permission-rule engine | L |
| P0.2 | Permission modes | S |
| P0.3 | Trust + config layering + grants | M |
| P1.1 | Hooks crate | M |
| P1.2 | OS sandbox | L |
| P1.3 | Project rules (AGENTS.md) | S–M |
| P2.1 | Worktree isolation | M |
| P2.2 | Subagent capability modes | S–M |
| P2.3 | Session resume/fork/rewind | L |
| P2.4 | ACP + headless | L |

## Top risks (consolidated)

- **Shell command parsing** (P0.1) — the correctness crux; adopt grok's
  "can't-split ⇒ prompt as one unit" rule.
- **Landlock kernel floor 5.13** (P1.2) — probe & fail-open for built-ins; may
  affect the release aarch64 leg; ship `workspace`/`read-only` before
  `strict`/custom-deny.
- **Config-format lock-in** (P0.3) — commit to a `[permission]`/`[sandbox]`
  schema early; support both structured and compact rule forms from day one.
- **Session disk growth** (P2.3) — rewind snapshots dominate; cap + compact.

---

*Grounded against Dadhichi v0.1.13. Source of best practices:
[xai-org/grok-build](https://github.com/xai-org/grok-build) user-guide docs
(permissions, hooks, sandbox, subagents, project-rules, sessions).*
