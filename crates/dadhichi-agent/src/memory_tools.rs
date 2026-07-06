//! Memory-access tools: how an agent (or a delegate) writes to and recalls from
//! shared [`Memory`].
//!
//! The agent framework already keeps layered [`Memory`], but until now it was
//! internal machinery the run drove for itself. These tools expose it as
//! ordinary [`Tool`]s so a model tool-loop — or a sub-agent spawned by the
//! `task` tool — can deliberately *remember* a fact and *recall* it later. The
//! store is shared behind a lock, so notes an agent writes are visible to the
//! delegates it spawns (state hand-off), while each run's conversation context
//! stays isolated.

use crate::memory::{Memory, Tier};
use async_trait::async_trait;
use dadhichi_mcp::{Tool, ToolError, ToolResult, ToolSpec};
use std::sync::{Arc, Mutex};

/// A [`Memory`] shared across the tools that read and write it.
pub type SharedMemory = Arc<Mutex<Memory>>;

/// Build a fresh shared memory store.
pub fn shared_memory() -> SharedMemory {
    Arc::new(Mutex::new(Memory::new()))
}

fn parse_tier(value: Option<&str>) -> Tier {
    match value {
        Some("working") => Tier::Working,
        Some("long_term") | Some("longterm") | Some("long-term") => Tier::LongTerm,
        // Default to the durable conversation tier — what an agent means by
        // "remember this" for the rest of the run.
        _ => Tier::Conversation,
    }
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Working => "working",
        Tier::Conversation => "conversation",
        Tier::LongTerm => "long_term",
    }
}

/// Records a fact into shared memory.
#[derive(Debug)]
pub struct MemoryWriteTool {
    memory: SharedMemory,
}

impl MemoryWriteTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "memory.write";

    /// Write into `memory`.
    pub fn new(memory: SharedMemory) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Remember a fact for later recall. Optional `tier`: working, \
                          conversation (default), or long_term."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "tier": { "type": "string", "enum": ["working", "conversation", "long_term"] }
                },
                "required": ["content"]
            }),
            // Recall/record of the agent's own notes needs no capability grant.
            permissions: vec![],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let content = args
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `content`".into()))?;
        let tier = parse_tier(args.get("tier").and_then(|t| t.as_str()));
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| ToolError::Execution("memory lock poisoned".into()))?;
        memory.remember(tier, content);
        Ok(serde_json::json!({
            "remembered": true,
            "tier": tier_name(tier),
            "total": memory.len(),
        }))
    }
}

/// Recalls facts from shared memory by keyword.
#[derive(Debug)]
pub struct MemoryRecallTool {
    memory: SharedMemory,
}

impl MemoryRecallTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "memory.recall";

    /// Recall from `memory`.
    pub fn new(memory: SharedMemory) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description:
                "Recall remembered facts whose text contains the query (case-insensitive).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            permissions: vec![],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `query`".into()))?;
        let memory = self
            .memory
            .lock()
            .map_err(|_| ToolError::Execution("memory lock poisoned".into()))?;
        let matches: Vec<serde_json::Value> = memory
            .recall(query)
            .into_iter()
            .map(
                |item| serde_json::json!({ "tier": tier_name(item.tier), "content": item.content }),
            )
            .collect();
        Ok(serde_json::json!({ "query": query, "matches": matches }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_recall_round_trips() {
        let memory = shared_memory();
        MemoryWriteTool::new(memory.clone())
            .invoke(serde_json::json!({
                "content": "the build uses cargo nextest",
                "tier": "long_term"
            }))
            .await
            .unwrap();

        let out = MemoryRecallTool::new(memory)
            .invoke(serde_json::json!({ "query": "nextest" }))
            .await
            .unwrap();
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["tier"], "long_term");
        assert!(matches[0]["content"].as_str().unwrap().contains("nextest"));
    }

    #[tokio::test]
    async fn shared_store_is_visible_across_tool_handles() {
        // A parent writes; a separately-constructed recall tool over the same
        // Arc sees it — this is how a delegate reads the caller's notes.
        let memory = shared_memory();
        MemoryWriteTool::new(memory.clone())
            .invoke(serde_json::json!({ "content": "shared fact" }))
            .await
            .unwrap();
        let out = MemoryRecallTool::new(memory)
            .invoke(serde_json::json!({ "query": "shared" }))
            .await
            .unwrap();
        assert_eq!(out["matches"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn write_requires_content() {
        let err = MemoryWriteTool::new(shared_memory())
            .invoke(serde_json::json!({ "tier": "working" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
