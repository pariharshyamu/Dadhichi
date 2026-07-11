//! Cross-invocation session memory for the CLI.
//!
//! Each `dadhichi <goal>` invocation boots a fresh kernel, so without this the
//! agent would forget everything the moment it exits — a follow-up like
//! "now add a scoreboard to it" would have no idea what "it" is. This module
//! persists the agent's memory to `.dadhichi/session.json` in the workspace and
//! reloads it on the next run, giving the one-shot CLI the same continuity the
//! long-lived TUI session enjoys.
//!
//! The store is a plain JSON array of [`MemoryItem`]s, capped to a recent window
//! so the file (and the context it seeds) can't grow without bound.

use dadhichi_agent::{Memory, MemoryItem, Tier};
use std::path::{Path, PathBuf};

/// The most recent memory items to keep across runs. Enough to carry the thread
/// of a conversation without letting the session file grow unbounded.
const MAX_PERSISTED_ITEMS: usize = 200;

/// The on-disk location of the session file for `workspace`.
pub fn session_path(workspace: &Path) -> PathBuf {
    workspace.join(".dadhichi").join("session.json")
}

/// Load persisted memory items for `workspace`, newest-last. Returns an empty
/// vec when there is no session yet or it can't be read/parsed — a corrupt or
/// missing session must never block a run.
pub fn load(workspace: &Path) -> Vec<MemoryItem> {
    let path = session_path(workspace);
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<MemoryItem>>(&bytes).unwrap_or_default()
}

/// Seed a fresh [`Memory`] from persisted items, marking every carried-over item
/// as durable [`LongTerm`](Tier::LongTerm) context (prior working/conversation
/// turns become background the new run can recall, not live scratch state).
pub fn seed_memory(items: &[MemoryItem]) -> Memory {
    let mut memory = Memory::new();
    for item in items {
        memory.remember(Tier::LongTerm, item.content.clone());
    }
    memory
}

/// A one-line, human-readable recap of the last session for the CLI banner, so
/// the user can see continuity was restored. `None` when there is nothing yet.
pub fn recap(items: &[MemoryItem]) -> Option<String> {
    let last = items.iter().rev().find(|i| !i.content.trim().is_empty())?;
    let text = last.content.trim();
    let clipped: String = text.chars().take(72).collect();
    Some(if clipped.len() < text.len() {
        format!("{clipped}…")
    } else {
        clipped
    })
}

/// Persist `memory` for `workspace`, keeping only the most recent
/// [`MAX_PERSISTED_ITEMS`]. Best-effort: a write failure is reported by the
/// caller but never aborts the run.
pub fn save(workspace: &Path, memory: &Memory) -> std::io::Result<()> {
    let mut items = memory.snapshot();
    if items.len() > MAX_PERSISTED_ITEMS {
        items = items.split_off(items.len() - MAX_PERSISTED_ITEMS);
    }
    let path = session_path(workspace);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(&items)?;
    std::fs::write(&path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_memory_across_a_workspace() {
        let dir = std::env::temp_dir().join(format!("dadhichi-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // First "invocation" records some context and saves it.
        let mut mem = Memory::new();
        mem.remember(Tier::Working, "goal: build a tic tac toe game in html");
        mem.remember(Tier::Conversation, "created index.html with the board");
        save(&dir, &mem).unwrap();

        // Second "invocation" loads it and can recall the earlier work.
        let loaded = load(&dir);
        assert_eq!(loaded.len(), 2);
        let seeded = seed_memory(&loaded);
        assert!(!seeded.recall("tic tac toe").is_empty());
        assert!(recap(&loaded).unwrap().contains("index.html"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_session_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join("dadhichi-session-does-not-exist-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).is_empty());
        assert!(recap(&load(&dir)).is_none());
    }

    #[test]
    fn save_caps_the_number_of_items() {
        let dir = std::env::temp_dir().join(format!("dadhichi-session-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut mem = Memory::new();
        for i in 0..(MAX_PERSISTED_ITEMS + 50) {
            mem.remember(Tier::Conversation, format!("turn {i}"));
        }
        save(&dir, &mem).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.len(), MAX_PERSISTED_ITEMS);
        // The most recent turns are the ones kept.
        assert!(loaded.last().unwrap().content.contains(&format!(
            "turn {}",
            MAX_PERSISTED_ITEMS + 50 - 1
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
