//! # dadhichi-mcp
//!
//! Dadhichi's **Model Context Protocol** layer and its permission-aware
//! [`ToolRegistry`]. Agents never touch a filesystem or a network socket
//! directly — they invoke [`Tool`]s through the registry, which enforces a
//! [`GrantSet`] at a single choke point. External MCP servers (GitHub, Docker,
//! databases) are bridged in behind the same [`Tool`] trait via [`McpClient`].
//!
//! ```
//! use dadhichi_mcp::{ToolRegistry, EchoTool, GrantSet};
//! use std::sync::Arc;
//!
//! # async fn demo() {
//! let registry = ToolRegistry::new();
//! registry.register(Arc::new(EchoTool));
//!
//! let out = registry
//!     .invoke("echo", serde_json::json!({ "value": "hi" }), &GrantSet::none())
//!     .await
//!     .unwrap();
//! assert_eq!(out["value"], "hi");
//! # }
//! ```

pub mod bridge;
pub mod catalog;
pub mod client;
pub mod connector;
pub mod protocol;
pub mod registry;
pub mod tool;

pub use bridge::McpToolBridge;
pub use catalog::{Connector, SecretRequirement, builtin_connectors, connector};
pub use client::McpConnection;
pub use connector::{
    ConnectReport, ConnectedServer, McpConnections, McpServerConfig, McpServersConfig, ServerError,
    connect_servers, resolve_env,
};
pub use protocol::{
    McpClient, McpError, PromptArgument, PromptSpec, ResourceSpec, RpcErrorObject, RpcRequest,
    RpcResponse, ServerCapabilities,
};
pub use registry::{GrantSet, ToolRegistry};
pub use tool::{EchoTool, Permission, Tool, ToolError, ToolResult, ToolSpec};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct WriterTool;

    #[async_trait]
    impl Tool for WriterTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fs.write".into(),
                description: "Write a file.".into(),
                input_schema: serde_json::json!({ "type": "object" }),
                permissions: vec![Permission::WriteWorkspace],
            }
        }
        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            Ok(serde_json::json!({ "written": true }))
        }
    }

    #[tokio::test]
    async fn permission_denied_without_grant() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(WriterTool));

        let err = registry
            .invoke("fs.write", serde_json::json!({}), &GrantSet::none())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn permission_granted_allows_invocation() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(WriterTool));

        let grants = GrantSet::from_iter([Permission::WriteWorkspace]);
        let out = registry
            .invoke("fs.write", serde_json::json!({}), &grants)
            .await
            .unwrap();
        assert_eq!(out["written"], true);
    }

    #[test]
    fn registry_lists_tools_sorted() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(WriterTool));
        let names: Vec<_> = registry.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["echo", "fs.write"]);
    }
}
