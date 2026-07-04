//! Bridging external MCP tools into the local [`ToolRegistry`](crate::ToolRegistry).
//!
//! An [`McpToolBridge`] wraps one tool exposed by a remote MCP server behind the
//! same [`Tool`] trait the agents already use. Once bridged and registered, an
//! agent invokes a GitHub or Docker MCP tool exactly as it invokes a built-in
//! one — permission-gated, uniform, and oblivious to the network hop.

use crate::client::McpConnection;
use crate::tool::{Tool, ToolError, ToolResult, ToolSpec};
use async_trait::async_trait;
use std::sync::Arc;

/// Adapts one remote MCP tool to the local [`Tool`] trait.
#[derive(Debug)]
pub struct McpToolBridge {
    connection: Arc<McpConnection>,
    spec: ToolSpec,
}

impl McpToolBridge {
    /// Wrap `spec` served by `connection`.
    pub fn new(connection: Arc<McpConnection>, spec: ToolSpec) -> Self {
        Self { connection, spec }
    }

    /// Discover every tool `connection` exposes and wrap each as a bridge,
    /// ready to register in a [`ToolRegistry`](crate::ToolRegistry).
    pub async fn discover(
        connection: Arc<McpConnection>,
    ) -> Result<Vec<McpToolBridge>, crate::protocol::McpError> {
        let specs = connection.list_tools().await?;
        Ok(specs
            .into_iter()
            .map(|spec| McpToolBridge::new(connection.clone(), spec))
            .collect())
    }
}

#[async_trait]
impl Tool for McpToolBridge {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        self.connection
            .call_tool(&self.spec.name, args)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{GrantSet, ToolRegistry};
    use crate::tool::Permission;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn mock_server(
        reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = writer;
        while let Ok(Some(line)) = lines.next_line().await {
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = req.get("id").cloned().unwrap();
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = match method {
                "tools/list" => serde_json::json!({
                    "tools": [{ "name": "github.create_issue", "description": "open an issue", "inputSchema": {} }]
                }),
                "tools/call" => serde_json::json!({ "issue": 42 }),
                _ => serde_json::Value::Null,
            };
            let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
            writer.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn discovered_mcp_tool_is_callable_through_the_registry() {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_side);
        let (s_read, s_write) = tokio::io::split(server_side);
        tokio::spawn(mock_server(s_read, s_write));

        let connection = Arc::new(McpConnection::new(c_read, c_write));
        let bridges = McpToolBridge::discover(connection).await.unwrap();
        assert_eq!(bridges.len(), 1);

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(bridges.into_iter().next().unwrap()));

        // The bridged tool requires Network permission (set by discovery).
        let denied = registry
            .invoke(
                "github.create_issue",
                serde_json::json!({}),
                &GrantSet::none(),
            )
            .await;
        assert!(denied.is_err());

        let grants = GrantSet::from_iter([Permission::Network]);
        let out = registry
            .invoke(
                "github.create_issue",
                serde_json::json!({ "title": "bug" }),
                &grants,
            )
            .await
            .unwrap();
        assert_eq!(out["issue"], 42);
    }
}
