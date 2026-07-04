//! Tools: the unit of capability an agent can invoke.
//!
//! A tool is a JSON-in/JSON-out function with a schema and a permission scope.
//! Tools may be built in (filesystem, git, terminal) or bridged in over the
//! Model Context Protocol from an external MCP server (GitHub, Docker, a
//! database, …). Either way agents see the same [`Tool`] trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors a tool invocation can produce.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The arguments did not match the tool's schema.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The caller lacked the permission this tool requires.
    #[error("permission denied: requires {0}")]
    PermissionDenied(String),
    /// The tool ran but failed.
    #[error("tool execution failed: {0}")]
    Execution(String),
}

/// Result of a tool call.
pub type ToolResult = Result<serde_json::Value, ToolError>;

/// A coarse permission scope gating a tool. The security layer maps these onto
/// user consent prompts and the agent sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Read files within the workspace.
    ReadWorkspace,
    /// Modify files within the workspace.
    WriteWorkspace,
    /// Execute shell/terminal commands.
    RunCommands,
    /// Make outbound network requests.
    Network,
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Permission::ReadWorkspace => "read_workspace",
            Permission::WriteWorkspace => "write_workspace",
            Permission::RunCommands => "run_commands",
            Permission::Network => "network",
        };
        f.write_str(s)
    }
}

/// Self-describing metadata for a tool, mirroring MCP's `tools/list` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique tool name, e.g. `"fs.read"` or `"github.create_issue"`.
    pub name: String,
    /// Human-readable description for the model and the UI.
    pub description: String,
    /// JSON Schema describing the accepted arguments.
    pub input_schema: serde_json::Value,
    /// Permissions the caller must hold to invoke this tool.
    pub permissions: Vec<Permission>,
}

/// An invokable capability.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's self-description.
    fn spec(&self) -> ToolSpec;

    /// Execute the tool against `args`.
    async fn invoke(&self, args: serde_json::Value) -> ToolResult;
}

/// A trivial built-in tool that echoes its arguments. Useful as a smoke test
/// and as a template for real tool implementations.
#[derive(Debug, Default)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Return the arguments unchanged.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
            permissions: vec![],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        Ok(args)
    }
}
