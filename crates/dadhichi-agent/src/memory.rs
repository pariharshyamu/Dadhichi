//! A small, layered memory store for agents.
//!
//! Real deployments back the long-term tier with a vector database (LanceDB)
//! for semantic recall; this in-memory implementation captures the tiering and
//! the retrieval API the rest of the agent framework depends on.

use serde::{Deserialize, Serialize};

/// Which tier a memory belongs to. Tiers differ in lifetime and eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Scratch space for the current step; cleared frequently.
    Working,
    /// The running conversation with the user.
    Conversation,
    /// Durable facts about the project and user preferences.
    LongTerm,
}

/// A single stored memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    /// The tier this item lives in.
    pub tier: Tier,
    /// The remembered text.
    pub content: String,
}

/// A simple append-and-recall memory store.
#[derive(Debug, Default)]
pub struct Memory {
    items: Vec<MemoryItem>,
}

impl Memory {
    /// Create an empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember `content` in `tier`.
    pub fn remember(&mut self, tier: Tier, content: impl Into<String>) {
        self.items.push(MemoryItem {
            tier,
            content: content.into(),
        });
    }

    /// Every item currently in `tier`.
    pub fn recall_tier(&self, tier: Tier) -> Vec<&MemoryItem> {
        self.items.iter().filter(|i| i.tier == tier).collect()
    }

    /// A naive keyword recall standing in for semantic (vector) search.
    ///
    /// Returns items whose content contains `query`, case-insensitively. The
    /// production implementation replaces this with embedding similarity.
    pub fn recall(&self, query: &str) -> Vec<&MemoryItem> {
        let needle = query.to_lowercase();
        self.items
            .iter()
            .filter(|i| i.content.to_lowercase().contains(&needle))
            .collect()
    }

    /// Clear the working tier (e.g. at the end of a step).
    pub fn clear_working(&mut self) {
        self.items.retain(|i| i.tier != Tier::Working);
    }

    /// Total number of stored items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
