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

    /// Clone the full item list — a checkpoint the caller can later
    /// [`restore`](Self::restore) after a failed or abandoned step.
    pub fn snapshot(&self) -> Vec<MemoryItem> {
        self.items.clone()
    }

    /// Replace all items with a previously taken [`snapshot`](Self::snapshot).
    pub fn restore(&mut self, items: Vec<MemoryItem>) {
        self.items = items;
    }

    /// Bound memory to at most `max` items, evicting the oldest first but never
    /// dropping `LongTerm` entries. Returns the number of items evicted.
    pub fn prune(&mut self, max: usize) -> usize {
        if self.items.len() <= max {
            return 0;
        }
        let mut removed = 0;
        // Walk oldest-first, dropping non-durable items until within budget.
        let target = self.items.len() - max;
        let mut kept = Vec::with_capacity(self.items.len());
        for item in std::mem::take(&mut self.items) {
            if removed < target && item.tier != Tier::LongTerm {
                removed += 1;
            } else {
                kept.push(item);
            }
        }
        self.items = kept;
        removed
    }

    /// Collapse every `Conversation` item into a single `LongTerm` summary,
    /// freeing working space while preserving the gist. `summarise_fn` receives
    /// the concatenated conversation text and returns the summary to store.
    pub fn summarise_conversation(&mut self, summarise_fn: impl Fn(&str) -> String) {
        let convo: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.tier == Tier::Conversation)
            .map(|i| i.content.clone())
            .collect();
        if convo.is_empty() {
            return;
        }
        let summary = summarise_fn(&convo.join("\n"));
        self.items.retain(|i| i.tier != Tier::Conversation);
        self.remember(Tier::LongTerm, summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_restore_round_trip() {
        let mut mem = Memory::new();
        mem.remember(Tier::Working, "a");
        let snap = mem.snapshot();
        mem.remember(Tier::Working, "b");
        assert_eq!(mem.len(), 2);
        mem.restore(snap);
        assert_eq!(mem.len(), 1);
    }

    #[test]
    fn prune_evicts_oldest_but_keeps_long_term() {
        let mut mem = Memory::new();
        mem.remember(Tier::LongTerm, "durable");
        mem.remember(Tier::Working, "old");
        mem.remember(Tier::Working, "new");
        let evicted = mem.prune(2);
        assert_eq!(evicted, 1);
        assert_eq!(mem.len(), 2);
        // The durable item survives; the oldest working item is gone.
        assert!(!mem.recall("durable").is_empty());
        assert!(mem.recall("old").is_empty());
    }

    #[test]
    fn summarise_collapses_conversation_into_long_term() {
        let mut mem = Memory::new();
        mem.remember(Tier::Conversation, "user asked X");
        mem.remember(Tier::Conversation, "assistant answered Y");
        mem.summarise_conversation(|text| format!("summary({} chars)", text.len()));

        assert!(mem.recall_tier(Tier::Conversation).is_empty());
        let long = mem.recall_tier(Tier::LongTerm);
        assert_eq!(long.len(), 1);
        assert!(long[0].content.starts_with("summary("));
    }
}
