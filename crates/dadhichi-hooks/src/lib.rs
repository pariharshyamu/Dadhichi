//! # dadhichi-hooks
//!
//! Lifecycle **hooks** for Dadhichi: user- or project-supplied scripts that run
//! at key moments in a session — a `SessionStart` setup step, a `PostToolUse`
//! logger, or a `PreToolUse` guard that can *deny* a dangerous tool call before
//! it runs.
//!
//! The design mirrors the grok-build model:
//!
//! - Hooks are declared in JSON files under `~/.dadhichi/hooks/` (always
//!   trusted) and `<project>/.dadhichi/hooks/` (gated on folder-trust, so an
//!   untrusted checkout can't run arbitrary code).
//! - Only [`PreToolUse`](event::HookEvent::PreToolUse) can block; every other
//!   event is passive.
//! - Hooks are **fail-open**: a crash, timeout, or malformed output never blocks
//!   a tool call. Only an explicit `{"decision":"deny"}` (or exit code 2) does.
//!
//! [`HookRunner`] implements [`PreToolUseGate`](dadhichi_mcp::PreToolUseGate),
//! so installing it on the tool registry wires `PreToolUse` hooks into the same
//! single choke point every tool call already passes through.
//!
//! ```no_run
//! use dadhichi_hooks::{discover, HookRunner};
//! use dadhichi_security::TrustStore;
//! use std::sync::Arc;
//!
//! # fn wire(registry: &dadhichi_mcp::ToolRegistry, cwd: &std::path::Path) {
//! let trust = TrustStore::at_home().unwrap();
//! let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
//! let (hooks, _warnings) = discover(home.as_deref(), cwd, &trust);
//! let runner = HookRunner::new(hooks, "session-id", cwd);
//! registry.set_pre_tool_use_gate(Arc::new(runner));
//! # }
//! ```

mod config;
mod event;
mod runner;

pub use config::{Handler, Hook, Source, discover, parse_file};
pub use event::HookEvent;
pub use runner::{HookPayload, HookRunner};
