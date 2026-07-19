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

/// Dadhichi's own bookkeeping (session memory, local config) — hidden from
/// listing/search tools so the agent doesn't waste context re-reading its own
/// serialized transcript. An explicit `fs.read` of such a path still works.
fn is_internal(path: &str) -> bool {
    let path = path.trim_start_matches(['/', '\\']);
    path.starts_with(".dadhichi/") || path.starts_with(".dadhichi\\") || path == ".dadhichi"
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

/// Reads larger than this many lines are paged by default, so one `fs.read`
/// can't flood the context with a file the model only needed a corner of.
const READ_PAGE_LINES: usize = 400;

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Read a file by path. Large files are paged: pass `offset` (0-based \
                          line) and `limit` to read a specific slice instead of re-reading \
                          everything."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "0-based first line" },
                    "limit": { "type": "integer", "description": "max lines to return" }
                },
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
        let offset = args.get("offset").and_then(|o| o.as_u64()).map(|o| o as usize);
        let limit = args.get("limit").and_then(|l| l.as_u64()).map(|l| l as usize);

        let total = content.lines().count();
        let explicit = offset.is_some() || limit.is_some();
        let start = offset.unwrap_or(0);
        let take = limit.unwrap_or(if explicit { READ_PAGE_LINES } else { total });

        // Small files (or explicit full requests within the page size) come
        // back whole; anything else is a slice with paging guidance.
        if !explicit && total <= READ_PAGE_LINES {
            return Ok(serde_json::json!({ "path": path, "content": content, "lines": total }));
        }
        let take = if explicit { take } else { READ_PAGE_LINES };
        let slice: String = content
            .lines()
            .skip(start)
            .take(take)
            .collect::<Vec<_>>()
            .join("\n");
        let end = (start + take).min(total);
        Ok(serde_json::json!({
            "path": path,
            "content": slice,
            "lines": total,
            "showing": format!("lines {}..{} of {}", start, end, total),
            "note": if end < total {
                format!("file continues — fs.read {{\"path\":\"{path}\",\"offset\":{end}}} for more, \
                         or fs.grep to jump straight to what you need")
            } else {
                String::new()
            },
        }))
    }
}

/// Surgical find-and-replace in one file — the token-cheap way to change code.
/// Sending only the changed snippet instead of rewriting the whole file with
/// `fs.write` keeps large-file edits from costing tens of thousands of tokens.
#[derive(Debug)]
pub struct FsEditTool {
    store: Arc<dyn StateStore>,
}

impl FsEditTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "fs.edit";

    /// Edit files in `store`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for FsEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Replace an exact text snippet in a file (surgical edit). `find` must \
                          match exactly once — include surrounding lines to disambiguate — or \
                          pass replace_all:true. Prefer this over fs.write for changes to \
                          existing files."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "find": { "type": "string" },
                    "replace": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "find", "replace"]
            }),
            permissions: vec![Permission::WriteWorkspace],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let path = path_arg(&args)?;
        let find = args
            .get("find")
            .and_then(|f| f.as_str())
            .filter(|f| !f.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `find`".into()))?;
        let replace = args
            .get("replace")
            .and_then(|r| r.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `replace`".into()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);

        let content = self
            .store
            .read(&path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let count = content.matches(find).count();
        if count == 0 {
            return Err(ToolError::Execution(format!(
                "`find` text not found in {path} — fs.read the relevant slice and match it \
                 exactly (whitespace included)"
            )));
        }
        if count > 1 && !replace_all {
            return Err(ToolError::Execution(format!(
                "`find` matches {count} places in {path} — include more surrounding context to \
                 make it unique, or pass replace_all:true"
            )));
        }
        let updated = if replace_all {
            content.replace(find, replace)
        } else {
            content.replacen(find, replace, 1)
        };
        self.store
            .write(&path, &updated)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(serde_json::json!({
            "path": path,
            "replacements": if replace_all { count } else { 1 },
            "bytes": updated.len(),
        }))
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
        let entries: Vec<String> = self
            .store
            .list(prefix)
            .map_err(|e| ToolError::Execution(e.to_string()))?
            .into_iter()
            .filter(|p| !is_internal(p))
            .collect();
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
            if is_internal(&path) {
                continue;
            }
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
            .filter(|path| !is_internal(path) && Self::matches(pattern, path))
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
    async fn large_reads_are_paged_and_slices_are_addressable() {
        let store = store();
        let big: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        store.write("big.txt", &big).unwrap();

        // Default read of a large file returns the first page plus guidance.
        let read = FsReadTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "big.txt" }))
            .await
            .unwrap();
        assert_eq!(read["lines"], 1000);
        let content = read["content"].as_str().unwrap();
        assert!(content.contains("line 0") && content.contains("line 399"));
        assert!(!content.contains("line 400"));
        assert!(read["note"].as_str().unwrap().contains("offset"));

        // An explicit slice returns exactly that window.
        let read = FsReadTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "big.txt", "offset": 500, "limit": 2 }))
            .await
            .unwrap();
        assert_eq!(read["content"], "line 500\nline 501");

        // Small files still come back whole, unpaged.
        store.write("small.txt", "just this").unwrap();
        let read = FsReadTool::new(store)
            .invoke(serde_json::json!({ "path": "small.txt" }))
            .await
            .unwrap();
        assert_eq!(read["content"], "just this");
    }

    #[tokio::test]
    async fn edit_replaces_exactly_and_refuses_ambiguity() {
        let store = store();
        store
            .write("app.py", "x = 1\ny = 1\nprint(x)\n")
            .unwrap();

        // Ambiguous find is refused with guidance.
        let err = FsEditTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "app.py", "find": "= 1", "replace": "= 2" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 places"));

        // A unique find edits in place.
        FsEditTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "app.py", "find": "x = 1", "replace": "x = 42" }))
            .await
            .unwrap();
        assert!(store.read("app.py").unwrap().contains("x = 42"));

        // replace_all handles the rest; a missing find errors clearly.
        FsEditTool::new(store.clone())
            .invoke(serde_json::json!({ "path": "app.py", "find": "= 1", "replace": "= 7", "replace_all": true }))
            .await
            .unwrap();
        assert!(store.read("app.py").unwrap().contains("y = 7"));
        let err = FsEditTool::new(store)
            .invoke(serde_json::json!({ "path": "app.py", "find": "zzz", "replace": "" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn dadhichi_internals_are_hidden_from_listing_and_search() {
        let store = store();
        for (path, content) in [
            (".dadhichi/session.json", "[{\"tier\":\"working\"}]"),
            ("src/main.rs", "fn main() {}"),
        ] {
            store.write(path, content).unwrap();
        }

        // fs.ls omits the session file so the agent never "discovers" it.
        let listed = FsListTool::new(store.clone())
            .invoke(serde_json::json!({}))
            .await
            .unwrap();
        let entries = listed["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "src/main.rs");

        // fs.grep and fs.glob skip it too.
        let hits = FsGrepTool::new(store.clone())
            .invoke(serde_json::json!({ "query": "tier" }))
            .await
            .unwrap();
        assert_eq!(hits["count"], 0);
        let globbed = FsGlobTool::new(store.clone())
            .invoke(serde_json::json!({ "pattern": "*.json" }))
            .await
            .unwrap();
        assert!(globbed["entries"].as_array().unwrap().is_empty());

        // An explicit read still works (deliberate access is fine).
        let read = FsReadTool::new(store)
            .invoke(serde_json::json!({ "path": ".dadhichi/session.json" }))
            .await
            .unwrap();
        assert!(read["content"].as_str().unwrap().contains("tier"));
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
