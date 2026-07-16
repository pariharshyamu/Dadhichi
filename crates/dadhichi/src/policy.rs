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
use dadhichi_security::{SandboxStatus, TrustStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Apply the OS sandbox for `cwd`, if one is requested via the `DADHICHI_SANDBOX`
/// environment variable or `[sandbox] profile` in config. Must be called
/// **before** the async runtime starts so every worker thread and child process
/// inherits the kernel confinement.
///
/// Returns a boot-banner note. A fail-closed error (e.g. a custom `deny` list
/// that can't be enforced) aborts the process rather than run unconfined.
pub fn apply_sandbox(cwd: &Path) -> Option<String> {
    let name = std::env::var("DADHICHI_SANDBOX")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| dadhichi_config::Config::load(cwd).sandbox)?;

    let profile = match dadhichi_security::parse_profile(&name) {
        Ok(p) => p,
        Err(e) => return Some(format!("sandbox: {e}")),
    };

    let home = std::env::var_os("HOME").map(PathBuf::from);
    match dadhichi_security::apply_sandbox(&profile, cwd, home.as_deref()) {
        Ok(SandboxStatus::Off) => None,
        Ok(SandboxStatus::Enforced { network_requested }) => {
            let net = if network_requested {
                " (network restriction requested but not yet enforced)"
            } else {
                ""
            };
            Some(format!("sandbox '{name}' enforced{net}"))
        }
        Ok(SandboxStatus::Unsupported) => Some(format!(
            "sandbox '{name}' requested but this kernel can't enforce it — continuing unconfined"
        )),
        Err(e) => {
            // Fail-closed: refuse to run rather than expose what the profile
            // was meant to confine.
            eprintln!("dadhichi ▸ sandbox '{name}' could not be applied: {e}");
            std::process::exit(1);
        }
    }
}

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
