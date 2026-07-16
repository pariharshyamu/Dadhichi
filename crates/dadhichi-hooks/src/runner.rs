//! Firing hooks: building the event envelope, running command handlers, and
//! aggregating a `PreToolUse` deny.

use crate::config::{Handler, Hook};
use crate::event::HookEvent;
use async_trait::async_trait;
use dadhichi_mcp::{GateDecision, PreToolUseGate};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

/// Context passed to a firing hook: what the event is about.
#[derive(Debug, Clone, Default)]
pub struct HookPayload {
    /// The tool involved, for tool events.
    pub tool_name: Option<String>,
    /// The tool's arguments, for tool events.
    pub tool_input: Option<Value>,
}

/// Runs a fixed set of discovered [`Hook`]s. Cheap to clone the context; the
/// hooks themselves are shared by reference during a fire.
#[derive(Debug, Clone)]
pub struct HookRunner {
    hooks: Vec<Hook>,
    session_id: String,
    workspace_root: PathBuf,
}

/// Environment variables the runner always injects; a hook's own `env` map may
/// not override them.
const RESERVED_ENV: &[&str] = &[
    "DADHICHI_HOOK_EVENT",
    "DADHICHI_SESSION_ID",
    "DADHICHI_WORKSPACE_ROOT",
    "CLAUDE_PROJECT_DIR",
];

impl HookRunner {
    /// Build a runner over `hooks` with a session id and workspace root used to
    /// populate the event envelope and hook environment.
    pub fn new(
        hooks: Vec<Hook>,
        session_id: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            hooks,
            session_id: session_id.into(),
            workspace_root: workspace_root.into(),
        }
    }

    /// Whether any hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// The number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Fire every hook registered for `event` that matches `payload`.
    ///
    /// Returns `Some(reason)` when a **`PreToolUse`** hook explicitly denied the
    /// call (the first such reason); otherwise `None`. Passive events always
    /// return `None`. All hook failures — spawn errors, timeouts, malformed
    /// output — are **fail-open** (they never produce a deny).
    pub async fn fire(&self, event: HookEvent, payload: &HookPayload) -> Option<String> {
        let envelope = self.envelope(event, payload);
        for hook in &self.hooks {
            if hook.event != event || !hook.matches(payload.tool_name.as_deref()) {
                continue;
            }
            match &hook.handler {
                Handler::Command {
                    command,
                    timeout,
                    env,
                } => {
                    let deny = run_command(
                        command,
                        *timeout,
                        env,
                        &envelope,
                        event,
                        &self.session_id,
                        &self.workspace_root,
                    )
                    .await;
                    if event.is_blocking() {
                        if let Some(reason) = deny {
                            return Some(reason);
                        }
                    }
                }
                Handler::Http { url, .. } => {
                    // HTTP delivery is not yet implemented; such hooks fail open.
                    tracing::debug!(url, "skipping http hook (delivery not implemented)");
                }
            }
        }
        None
    }

    fn envelope(&self, event: HookEvent, payload: &HookPayload) -> Vec<u8> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let value = json!({
            "hookEventName": event.as_env(),
            "sessionId": self.session_id,
            "cwd": self.workspace_root,
            "workspaceRoot": self.workspace_root,
            "toolName": payload.tool_name,
            "toolInput": payload.tool_input,
            "timestamp": ts,
        });
        serde_json::to_vec(&value).unwrap_or_default()
    }
}

#[async_trait]
impl PreToolUseGate for HookRunner {
    async fn check(&self, tool: &str, args: &Value) -> GateDecision {
        let payload = HookPayload {
            tool_name: Some(tool.to_string()),
            tool_input: Some(args.clone()),
        };
        match self.fire(HookEvent::PreToolUse, &payload).await {
            Some(reason) => GateDecision::Deny(reason),
            None => GateDecision::Proceed,
        }
    }
}

/// Run one command hook. Returns `Some(reason)` only on an explicit deny (a
/// `{"decision":"deny"}` on stdout, or exit code 2). Every failure path returns
/// `None` (fail-open).
async fn run_command(
    command: &str,
    timeout: Duration,
    env: &std::collections::HashMap<String, String>,
    envelope: &[u8],
    event: HookEvent,
    session_id: &str,
    workspace_root: &std::path::Path,
) -> Option<String> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("DADHICHI_HOOK_EVENT", event.as_env())
        .env("DADHICHI_SESSION_ID", session_id)
        .env("DADHICHI_WORKSPACE_ROOT", workspace_root)
        .env("CLAUDE_PROJECT_DIR", workspace_root);
    for (k, v) in env {
        if !RESERVED_ENV.contains(&k.as_str()) {
            cmd.env(k, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, command, "hook failed to spawn (fail-open)");
            return None;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(envelope).await;
        // Drop closes the pipe so the hook sees EOF.
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(%e, command, "hook errored (fail-open)");
            return None;
        }
        Err(_) => {
            tracing::warn!(command, "hook timed out (fail-open)");
            return None;
        }
    };

    if !event.is_blocking() {
        return None; // passive event: output ignored
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    command_decision(output.status.code(), &stdout)
}

/// Interpret a blocking hook's result: an explicit `deny` decision on stdout
/// (honoured regardless of exit code), else exit code 2, else allow.
fn command_decision(exit_code: Option<i32>, stdout: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) {
        match v.get("decision").and_then(|d| d.as_str()) {
            Some("deny") => {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("denied by hook")
                    .to_string();
                return Some(reason);
            }
            Some("allow") => return None,
            _ => {}
        }
    }
    if exit_code == Some(2) {
        return Some("denied by hook (exit 2)".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Handler, Hook, Source};

    fn command_hook(event: HookEvent, command: &str) -> Hook {
        Hook {
            event,
            matcher: None,
            handler: Handler::Command {
                command: command.to_string(),
                timeout: Duration::from_secs(5),
                env: Default::default(),
            },
            source: Source::Global,
            name: "test".into(),
        }
    }

    fn runner(hooks: Vec<Hook>) -> HookRunner {
        HookRunner::new(hooks, "sess-1", std::env::temp_dir())
    }

    #[tokio::test]
    async fn deny_via_stdout_json_blocks() {
        let hook = command_hook(
            HookEvent::PreToolUse,
            r#"echo '{"decision":"deny","reason":"nope"}'"#,
        );
        let deny = runner(vec![hook])
            .fire(HookEvent::PreToolUse, &HookPayload::default())
            .await;
        assert_eq!(deny.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn exit_two_blocks() {
        let hook = command_hook(HookEvent::PreToolUse, "exit 2");
        let deny = runner(vec![hook])
            .fire(HookEvent::PreToolUse, &HookPayload::default())
            .await;
        assert!(deny.is_some());
    }

    #[tokio::test]
    async fn allow_or_silence_proceeds() {
        let hook = command_hook(HookEvent::PreToolUse, "true");
        let deny = runner(vec![hook])
            .fire(HookEvent::PreToolUse, &HookPayload::default())
            .await;
        assert!(deny.is_none());
    }

    #[tokio::test]
    async fn crashing_hook_fails_open() {
        // A command that doesn't exist can't spawn a shell failure we can rely
        // on cross-platform, so use a shell that exits non-zero WITHOUT a deny
        // decision — that must fail open (proceed), not block.
        let hook = command_hook(HookEvent::PreToolUse, "echo oops; exit 1");
        let deny = runner(vec![hook])
            .fire(HookEvent::PreToolUse, &HookPayload::default())
            .await;
        assert!(deny.is_none(), "non-2 exit without deny must proceed");
    }

    #[tokio::test]
    async fn matcher_scopes_the_hook() {
        let mut hook = command_hook(HookEvent::PreToolUse, "exit 2");
        hook.matcher = Some(regex::Regex::new("Bash").unwrap());
        let r = runner(vec![hook]);

        // Matches the tool → blocks.
        let payload = HookPayload {
            tool_name: Some("Bash".into()),
            tool_input: None,
        };
        assert!(r.fire(HookEvent::PreToolUse, &payload).await.is_some());

        // Different tool → hook doesn't fire.
        let payload = HookPayload {
            tool_name: Some("Read".into()),
            tool_input: None,
        };
        assert!(r.fire(HookEvent::PreToolUse, &payload).await.is_none());
    }

    #[tokio::test]
    async fn passive_event_never_denies() {
        // Even an exit-2 hook on a passive event doesn't block.
        let hook = command_hook(HookEvent::PostToolUse, "exit 2");
        let deny = runner(vec![hook])
            .fire(HookEvent::PostToolUse, &HookPayload::default())
            .await;
        assert!(deny.is_none());
    }
}
