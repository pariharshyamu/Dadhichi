//! `code.check` — fast, dependency-free structural verification for the agent.
//!
//! The ReAct loop invokes this automatically after every `fs.write`/`fs.edit`
//! and feeds the problems straight back to the model, closing the loop between
//! "I changed the file" and "the file is actually well-formed". Checks are
//! intentionally cheap (no compilers): HTML tag structure via the same checker
//! the Problems panel uses, JSON well-formedness via serde. Unsupported file
//! types report `supported: false` so the loop knows nothing was verified.

use crate::htmlcheck;
use async_trait::async_trait;
use dadhichi_mcp::{Permission, StateStore, Tool, ToolError, ToolResult, ToolSpec};
use std::sync::Arc;

/// Structural checks over the agent's sandboxed filesystem.
#[derive(Debug)]
pub struct CodeCheckTool {
    store: Arc<dyn StateStore>,
}

impl CodeCheckTool {
    /// The registry name the ReAct loop looks for.
    pub const NAME: &'static str = "code.check";

    /// Check files in `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CodeCheckTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Cheap structural check of one file (HTML tag structure, JSON \
                          well-formedness). Runs automatically after your edits; call it \
                          yourself instead of re-reading a file to confirm a change."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            permissions: vec![Permission::ReadWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let path = args
            .get("path")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `path`".into()))?;
        let text = self
            .store
            .read(path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let ext = path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let problems: Vec<serde_json::Value> = match ext.as_str() {
            "html" | "htm" => htmlcheck::diagnostics(&text)
                .into_iter()
                .map(|(line, message)| {
                    serde_json::json!({ "line": line + 1, "message": message })
                })
                .collect(),
            "json" => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(_) => Vec::new(),
                Err(err) => vec![serde_json::json!({
                    "line": err.line(),
                    "message": format!("invalid JSON: {err}"),
                })],
            },
            _ => {
                return Ok(serde_json::json!({
                    "path": path,
                    "supported": false,
                    "note": "no structural checker for this file type",
                }));
            }
        };
        Ok(serde_json::json!({
            "path": path,
            "supported": true,
            "count": problems.len(),
            "problems": problems,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_mcp::MemStore;

    fn store() -> Arc<dyn StateStore> {
        Arc::new(MemStore::new())
    }

    #[tokio::test]
    async fn flags_broken_html_and_json_and_passes_clean_files() {
        let store = store();
        store.write("bad.html", "<div><p>x</span>").unwrap();
        store.write("good.json", "{\"a\":1}").unwrap();
        store.write("bad.json", "{oops").unwrap();
        store.write("code.rs", "fn main() {}").unwrap();

        let tool = CodeCheckTool::new(store);
        let bad = tool.invoke(serde_json::json!({ "path": "bad.html" })).await.unwrap();
        assert!(bad["count"].as_u64().unwrap() >= 1);

        let good = tool.invoke(serde_json::json!({ "path": "good.json" })).await.unwrap();
        assert_eq!(good["count"], 0);

        let badj = tool.invoke(serde_json::json!({ "path": "bad.json" })).await.unwrap();
        assert_eq!(badj["count"], 1);

        // Unsupported types say so instead of pretending they were verified.
        let rs = tool.invoke(serde_json::json!({ "path": "code.rs" })).await.unwrap();
        assert_eq!(rs["supported"], false);
    }
}
