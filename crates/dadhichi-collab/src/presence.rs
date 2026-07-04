//! Shared cursors and presence.
//!
//! Presence is ephemeral, per-participant state — where each collaborator's
//! cursor is and who they are — kept separate from the durable document CRDT.
//! It is last-writer-wins per site: each update simply replaces that site's
//! entry, which is all convergence requires for transient state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One collaborator's live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// Display name.
    pub name: String,
    /// Cursor position as a character index into the document.
    pub cursor: usize,
    /// Whether this peer is an AI participant rather than a human.
    pub is_ai: bool,
}

/// The set of collaborators currently present.
#[derive(Debug, Default)]
pub struct Presence {
    peers: BTreeMap<u64, Peer>,
}

impl Presence {
    /// Create an empty presence set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a peer's state (last-writer-wins for `site`).
    pub fn update(&mut self, site: u64, peer: Peer) {
        self.peers.insert(site, peer);
    }

    /// Move a peer's cursor, leaving name/identity unchanged. No-op if the peer
    /// is not present.
    pub fn move_cursor(&mut self, site: u64, cursor: usize) {
        if let Some(peer) = self.peers.get_mut(&site) {
            peer.cursor = cursor;
        }
    }

    /// Remove a peer that has left.
    pub fn leave(&mut self, site: u64) {
        self.peers.remove(&site);
    }

    /// A peer by site id.
    pub fn peer(&self, site: u64) -> Option<&Peer> {
        self.peers.get(&site)
    }

    /// Every present peer, ordered by site id.
    pub fn peers(&self) -> impl Iterator<Item = (&u64, &Peer)> {
        self.peers.iter()
    }

    /// Number of present collaborators.
    pub fn count(&self) -> usize {
        self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human(name: &str, cursor: usize) -> Peer {
        Peer {
            name: name.into(),
            cursor,
            is_ai: false,
        }
    }

    #[test]
    fn tracks_peers_and_cursors() {
        let mut presence = Presence::new();
        presence.update(1, human("Ada", 0));
        presence.update(
            2,
            Peer {
                name: "Copilot".into(),
                cursor: 5,
                is_ai: true,
            },
        );
        assert_eq!(presence.count(), 2);

        presence.move_cursor(1, 10);
        assert_eq!(presence.peer(1).unwrap().cursor, 10);
        assert!(presence.peer(2).unwrap().is_ai);

        presence.leave(1);
        assert_eq!(presence.count(), 1);
        assert!(presence.peer(1).is_none());
    }

    #[test]
    fn update_is_last_writer_wins() {
        let mut presence = Presence::new();
        presence.update(1, human("Ada", 0));
        presence.update(1, human("Ada Lovelace", 3));
        assert_eq!(presence.count(), 1);
        assert_eq!(presence.peer(1).unwrap().name, "Ada Lovelace");
    }
}
