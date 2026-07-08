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

/// Searches the workspace/state store for a substring across file contents —
/// the agent's `grep`. Matching is a plain case-sensitive substring test (no
/// regex), scoped to files under an optional path prefix, so an agent can find
/// where a symbol or string lives without shelling out.
#[derive(Debug)]
pub struct FsGrepTool {
    store: Arc<dyn StateStore>,
}

impl FsGrepTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.grep";

    /// Search over `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for FsGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Search workspace file contents for a substring (like grep). Returns \
                          matching path:line:text hits, optionally scoped to a path prefix."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "substring to search for" },
                    "prefix": { "type": "string", "description": "optional path prefix to limit the search" },
                    "max_results": { "type": "integer", "description": "cap on hits returned (default 100)" }
                },
                "required": ["query"]
            }),
            permissions: vec![Permission::ReadWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `query`".into()))?;
        let prefix = args.get("prefix").and_then(|p| p.as_str()).unwrap_or("");
        let max_results = args
            .get("max_results")
            .and_then(|m| m.as_u64())
            .unwrap_or(100) as usize;

        let paths = self
            .store
            .list(prefix)
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut hits = Vec::new();
        'outer: for path in paths {
            // A file that can't be read (binary, gone) is skipped, not fatal.
            let Ok(content) = self.store.read(&path) else {
                continue;
            };
            for (n, line) in content.lines().enumerate() {
                if line.contains(query) {
                    hits.push(serde_json::json!({
                        "path": path,
                        "line": n + 1,
                        "text": line.trim_end(),
                    }));
                    if hits.len() >= max_results {
                        break 'outer;
                    }
                }
            }
        }

        Ok(serde_json::json!({
            "query": query,
            "count": hits.len(),
            "matches": hits,
        }))
    }
}

/// Lists workspace paths whose filename matches a glob-ish pattern — the agent's
/// `glob`/find. Supports `*` (any run of non-separator chars) and `?` (one
/// char); a pattern with no wildcard matches by substring, so `main.rs` finds
/// `src/main.rs`.
#[derive(Debug)]
pub struct FsGlobTool {
    store: Arc<dyn StateStore>,
}

impl FsGlobTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.glob";

    /// Match over `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }

    /// Whether `path` matches `pattern`, matching against the full path. `*`
    /// spans any characters (including `/`) and `?` one character; a wildcard-free
    /// pattern is treated as a substring match.
    fn matches(pattern: &str, path: &str) -> bool {
        if !pattern.contains(['*', '?']) {
            return path.contains(pattern);
        }
        // Classic two-pointer glob with backtracking on `*`.
        let (p, s) = (pattern.as_bytes(), path.as_bytes());
        let (mut pi, mut si) = (0usize, 0usize);
        let (mut star, mut mark) = (None, 0usize);
        while si < s.len() {
            if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
                pi += 1;
                si += 1;
            } else if pi < p.len() && p[pi] == b'*' {
                star = Some(pi);
                mark = si;
                pi += 1;
            } else if let Some(sp) = star {
                pi = sp + 1;
                mark += 1;
                si = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == b'*' {
            pi += 1;
        }
        pi == p.len()
    }
}

#[async_trait]
impl Tool for FsGlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Find workspace files by name pattern (like glob). Supports * and ? \
                          wildcards; a plain string matches by substring."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "e.g. *.rs, src/**/mod.rs, main.rs" }
                },
                "required": ["pattern"]
            }),
            permissions: vec![Permission::ReadWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `pattern`".into()))?;
        let entries: Vec<String> = self
            .store
            .list("")
            .map_err(|e| ToolError::Execution(e.to_string()))?
            .into_iter()
            .filter(|path| Self::matches(pattern, path))
            .collect();
        Ok(serde_json::json!({
            "pattern": pattern,
            "count": entries.len(),
            "entries": entries,
        }))
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

    #[tokio::test]
    async fn grep_finds_matching_lines_with_positions() {
        let store = store();
        store.write("src/a.rs", "fn main() {\n    let x = 1;\n}").unwrap();
        store.write("src/b.rs", "fn helper() {}").unwrap();

        let out = FsGrepTool::new(store.clone())
            .invoke(serde_json::json!({ "query": "fn " }))
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        // Reports path and 1-based line number for each hit.
        let matches = out["matches"].as_array().unwrap();
        assert!(matches.iter().any(|m| m["path"] == "src/a.rs" && m["line"] == 1));
        assert!(matches.iter().any(|m| m["path"] == "src/b.rs" && m["line"] == 1));

        // A prefix scopes the search.
        let scoped = FsGrepTool::new(store)
            .invoke(serde_json::json!({ "query": "fn ", "prefix": "src/b" }))
            .await
            .unwrap();
        assert_eq!(scoped["count"], 1);
    }

    #[tokio::test]
    async fn grep_missing_query_is_an_argument_error() {
        let err = FsGrepTool::new(store())
            .invoke(serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn glob_matches_wildcards_and_substrings() {
        let store = store();
        store.write("src/main.rs", "").unwrap();
        store.write("src/lib.rs", "").unwrap();
        store.write("README.md", "").unwrap();

        let rs = FsGlobTool::new(store.clone())
            .invoke(serde_json::json!({ "pattern": "*.rs" }))
            .await
            .unwrap();
        assert_eq!(rs["count"], 2, "*.rs matches both Rust files");

        let named = FsGlobTool::new(store.clone())
            .invoke(serde_json::json!({ "pattern": "main.rs" }))
            .await
            .unwrap();
        assert_eq!(named["count"], 1, "substring pattern finds src/main.rs");

        let q = FsGlobTool::new(store)
            .invoke(serde_json::json!({ "pattern": "src/li?.rs" }))
            .await
            .unwrap();
        assert_eq!(q["count"], 1, "? matches exactly one char");
    }
}
