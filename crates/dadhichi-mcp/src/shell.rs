//! A built-in tool that runs a shell command.
//!
//! [`TerminalTool`] is the canonical consequential capability: it requires
//! [`Permission::RunCommands`], so under an interrupting
//! [`ApprovalPolicy`](crate::ApprovalPolicy) every invocation pauses for a
//! human `y/n` before anything executes. It runs the command through the
//! platform shell and returns the exit status with captured `stdout`/`stderr`.

use crate::tool::{Permission, Tool, ToolError, ToolResult, ToolSpec};
use async_trait::async_trait;

/// Runs a single shell command and reports its result.
#[derive(Debug, Default)]
pub struct TerminalTool;

impl TerminalTool {
    /// The registry name this tool is invoked under.
    pub const NAME: &'static str = "terminal.run";
}

#[async_trait]
impl Tool for TerminalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Run a shell command and capture its output. Requires approval when the \
                          run-commands permission is set to interrupt."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line to execute." }
                },
                "required": ["command"]
            }),
            permissions: vec![Permission::RunCommands],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `command`".into()))?;

        // Use the platform shell so pipes/redirects behave as a user expects.
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        let output = cmd
            .output()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to spawn `{command}`: {e}")))?;

        Ok(serde_json::json!({
            "command": command,
            "status": output.status.code(),
            "success": output.status.success(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalPolicy, Decision, FixedApprover, PermissionMode};
    use crate::registry::{GrantSet, ToolRegistry};
    use std::sync::Arc;

    #[tokio::test]
    async fn runs_a_command_and_captures_output() {
        let out = TerminalTool
            .invoke(serde_json::json!({ "command": "echo hello" }))
            .await
            .unwrap();
        assert_eq!(out["success"], true);
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn interrupt_denied_blocks_the_run() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(TerminalTool));
        registry.set_policy(
            ApprovalPolicy::default().with(Permission::RunCommands, PermissionMode::Interrupt),
        );
        registry.set_approver(Arc::new(FixedApprover(Decision::Deny)));

        let grants = GrantSet::from_iter([Permission::RunCommands]);
        let err = registry
            .invoke(
                TerminalTool::NAME,
                serde_json::json!({ "command": "echo nope" }),
                &grants,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Rejected(_)));
    }

    #[tokio::test]
    async fn interrupt_approved_lets_the_run_through() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(TerminalTool));
        registry.set_policy(
            ApprovalPolicy::default().with(Permission::RunCommands, PermissionMode::Interrupt),
        );
        registry.set_approver(Arc::new(FixedApprover(Decision::Approve)));

        let grants = GrantSet::from_iter([Permission::RunCommands]);
        let out = registry
            .invoke(
                TerminalTool::NAME,
                serde_json::json!({ "command": "echo yes" }),
                &grants,
            )
            .await
            .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("yes"));
    }
}
