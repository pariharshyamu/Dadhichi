//! Boot-time activation of the layered permission config and lifecycle hooks.
//!
//! Everything the P0 permission engine and the P1 hooks runtime provide is
//! inert until something pushes it into the live [`ToolRegistry`]. This module
//! is that single wiring point: at startup it loads the layered
//! [`Config`](dadhichi_config::Config) (permission rules + session mode) and
//! discovers lifecycle hooks for the working directory, applying both to the
//! registry. It returns any non-fatal notes (bad rules, untrusted project
//! hooks) for the boot banner.

use dadhichi_hooks::HookRunner;
use dadhichi_mcp::ToolRegistry;
use dadhichi_security::TrustStore;
use std::path::Path;
use std::sync::Arc;

/// Apply layered config and discovered hooks to `tools` for `cwd`. `session_id`
/// labels the session in hook payloads. Returns human-readable notes to print.
pub fn activate(tools: &Arc<ToolRegistry>, cwd: &Path, session_id: &str) -> Vec<String> {
    let mut notes = Vec::new();

    // 1. Permission rules + session mode from ~/.dadhichi, the project chain,
    //    and /etc/dadhichi/requirements.toml. Empty config leaves the registry
    //    exactly as it was (default mode, no rules).
    let config = dadhichi_config::Config::load(cwd);
    if !config.rules.is_empty() {
        tools.set_rules(config.rules);
    }
    tools.set_mode(config.mode);
    for w in config.warnings {
        notes.push(format!("config: {w}"));
    }

    // 2. Lifecycle hooks. Global hooks always load; project hooks require an
    //    explicit folder-trust decision. Installing the runner as the
    //    pre-tool-use gate wires PreToolUse hooks into the registry choke point.
    if let Some(trust) = TrustStore::at_home() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let (hooks, warnings) = dadhichi_hooks::discover(home.as_deref(), cwd, &trust);
        for w in warnings {
            notes.push(format!("hooks: {w}"));
        }
        if !hooks.is_empty() {
            let count = hooks.len();
            let runner = HookRunner::new(hooks, session_id.to_string(), cwd);
            tools.set_pre_tool_use_gate(Arc::new(runner));
            notes.push(format!("hooks: {count} active"));
        }
    }

    notes
}
