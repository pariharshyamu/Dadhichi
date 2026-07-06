//! Virtual-filesystem tools: how an agent reads and writes its
//! [`StateStore`](crate::state::StateStore).
//!
//! These are the context-offloading surface deep agents rely on — write a plan
//! or an intermediate result to a path, read it back later, list what's there —
//! so the working set lives in files instead of ballooning the prompt. Each is
//! an ordinary permission-gated [`Tool`], so `fs.write` passes through the same
//! approval gate as any other workspace mutation, and a sandboxed
//! [`WorkspaceStore`](crate::state::WorkspaceStore) keeps every path inside the
//! project root.

use crate::state::StateStore;
use crate::tool::{Permission, Tool, ToolError, ToolResult, ToolSpec};
use async_trait::async_trait;
use std::sync::Arc;

fn path_arg(args: &serde_json::Value) -> Result<String, ToolError> {
    args.get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .ok_or_else(|| ToolError::InvalidArguments("missing `path`".into()))
}

/// Reads a file from the agent's state store.
#[derive(Debug)]
pub struct FsReadTool {
    store: Arc<dyn StateStore>,
}

impl FsReadTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.read";

    /// Read from `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Read a file from the workspace/state store by path.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            permissions: vec![Permission::ReadWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let path = path_arg(&args)?;
        let content = self
            .store
            .read(&path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(serde_json::json!({ "path": path, "content": content }))
    }
}

/// Writes a file into the agent's state store.
#[derive(Debug)]
pub struct FsWriteTool {
    store: Arc<dyn StateStore>,
}

impl FsWriteTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.write";

    /// Write into `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for FsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Write a file into the workspace/state store. Requires approval when \
                          write-workspace is set to interrupt."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            permissions: vec![Permission::WriteWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let path = path_arg(&args)?;
        let content = args
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `content`".into()))?;
        self.store
            .write(&path, content)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(serde_json::json!({ "path": path, "bytes": content.len() }))
    }
}

/// Lists the paths in the agent's state store, optionally under a prefix.
#[derive(Debug)]
pub struct FsListTool {
    store: Arc<dyn StateStore>,
}

impl FsListTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.ls";

    /// List `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for FsListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "List files in the workspace/state store, optionally filtered by a path \
                          prefix."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "prefix": { "type": "string" } }
            }),
            permissions: vec![Permission::ReadWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let prefix = args.get("prefix").and_then(|p| p.as_str()).unwrap_or("");
        let entries = self
            .store
            .list(prefix)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(serde_json::json!({ "prefix": prefix, "entries": entries }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MemStore;

    fn store() -> Arc<dyn StateStore> {
        Arc::new(MemStore::new())
    }

    #[tokio::test]
    async fn write_then_read_then_list() {
        let store = store();
        FsWriteTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "notes/a.md", "content": "hello" }))
            .await
            .unwrap();

        let read = FsReadTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "notes/a.md" }))
            .await
            .unwrap();
        assert_eq!(read["content"], "hello");

        let listed = FsListTool::new(store)
            .invoke(serde_json::json!({ "prefix": "notes/" }))
            .await
            .unwrap();
        assert_eq!(listed["entries"][0], "notes/a.md");
    }

    #[tokio::test]
    async fn read_missing_is_an_execution_error() {
        let err = FsReadTool::new(store())
            .invoke(serde_json::json!({ "path": "nope.txt" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }
}
