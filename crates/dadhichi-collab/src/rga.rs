//! A Replicated Growable Array (RGA) — a sequence CRDT for collaborative text.
//!
//! Every inserted character gets a globally unique [`Id`] `(counter, site)` and
//! remembers the id it was inserted *after* (its origin). Concurrent inserts at
//! the same origin are ordered deterministically by id, so every replica that
//! has seen the same set of operations reconstructs the identical string —
//! regardless of the order the operations arrived in. Deletes are tombstones,
//! making them idempotent and commutative. Together these give **strong
//! eventual consistency** with no central server.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A globally unique identifier for a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Id {
    /// A per-site Lamport-style counter.
    pub counter: u64,
    /// The originating replica's id.
    pub site: u64,
}

/// A collaborative operation, exchanged between replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Insert `ch` with `id`, positioned immediately after `origin`
    /// (`None` = the start of the document).
    Insert {
        /// The new character's id.
        id: Id,
        /// The id this character follows, or `None` for the document start.
        origin: Option<Id>,
        /// The character.
        ch: char,
    },
    /// Tombstone the character `id`.
    Delete {
        /// The id to delete.
        id: Id,
    },
}

#[derive(Debug, Clone)]
struct Elem {
    id: Id,
    origin: Option<Id>,
    ch: char,
    deleted: bool,
}

/// One replica of a shared text document.
#[derive(Debug)]
pub struct Rga {
    site: u64,
    clock: u64,
    elems: HashMap<Id, Elem>,
    /// origin id (or `None` for root) → child ids, kept sorted so traversal is
    /// deterministic across replicas.
    children: HashMap<Option<Id>, Vec<Id>>,
    /// Operations whose origin has not yet arrived, retried on each apply.
    pending: Vec<Op>,
}

impl Rga {
    /// Create an empty document owned by replica `site`.
    pub fn new(site: u64) -> Self {
        Self {
            site,
            clock: 0,
            elems: HashMap::new(),
            children: HashMap::new(),
            pending: Vec::new(),
        }
    }

    /// The current visible text.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.append_children(None, &mut out);
        out
    }

    /// The number of visible characters.
    pub fn len(&self) -> usize {
        self.visible_ids().len()
    }

    /// Whether the document is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert `ch` at visible `index`, returning the [`Op`] to broadcast.
    pub fn insert(&mut self, index: usize, ch: char) -> Op {
        let origin = index
            .checked_sub(1)
            .and_then(|i| self.visible_ids().get(i).copied());
        self.clock += 1;
        let id = Id {
            counter: self.clock,
            site: self.site,
        };
        let op = Op::Insert { id, origin, ch };
        self.apply(op.clone());
        op
    }

    /// Delete the character at visible `index`, returning the [`Op`] to
    /// broadcast (or `None` if the index is out of range).
    pub fn delete(&mut self, index: usize) -> Option<Op> {
        let id = *self.visible_ids().get(index)?;
        let op = Op::Delete { id };
        self.apply(op.clone());
        Some(op)
    }

    /// Apply an operation (local or remote). Idempotent and order-independent.
    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Insert { id, origin, ch } => {
                if self.elems.contains_key(&id) {
                    return; // already integrated
                }
                // Buffer inserts whose origin has not arrived yet.
                if let Some(origin_id) = origin
                    && !self.elems.contains_key(&origin_id)
                {
                    self.pending.push(Op::Insert { id, origin, ch });
                    return;
                }
                self.integrate(Elem {
                    id,
                    origin,
                    ch,
                    deleted: false,
                });
                self.keep_local_clock_ahead(id.counter);
                self.drain_pending();
            }
            Op::Delete { id } => {
                if let Some(elem) = self.elems.get_mut(&id) {
                    elem.deleted = true;
                } else {
                    self.pending.push(Op::Delete { id });
                }
            }
        }
    }

    fn integrate(&mut self, elem: Elem) {
        let origin = elem.origin;
        let id = elem.id;
        self.elems.insert(id, elem);
        let siblings = self.children.entry(origin).or_default();
        // Insert keeping the list sorted by id descending, so a newer insert at
        // the same origin lands immediately after the origin (closest first).
        let pos = siblings.partition_point(|&other| other > id);
        siblings.insert(pos, id);
    }

    fn drain_pending(&mut self) {
        // Retry buffered ops until no more can be integrated.
        loop {
            let ready: Vec<Op> = std::mem::take(&mut self.pending);
            let before = ready.len();
            let mut still_pending = Vec::new();
            for op in ready {
                match &op {
                    Op::Insert {
                        origin: Some(o), ..
                    } if !self.elems.contains_key(o) => still_pending.push(op),
                    Op::Delete { id } if !self.elems.contains_key(id) => still_pending.push(op),
                    _ => self.apply(op),
                }
            }
            let progressed = still_pending.len() < before;
            self.pending = still_pending;
            if !progressed || self.pending.is_empty() {
                break;
            }
        }
    }

    fn keep_local_clock_ahead(&mut self, seen: u64) {
        if seen > self.clock {
            self.clock = seen;
        }
    }

    fn visible_ids(&self) -> Vec<Id> {
        let mut out = Vec::new();
        self.collect_children(None, &mut out);
        out
    }

    fn collect_children(&self, origin: Option<Id>, out: &mut Vec<Id>) {
        if let Some(children) = self.children.get(&origin) {
            for &child in children {
                let elem = &self.elems[&child];
                if !elem.deleted {
                    out.push(child);
                }
                self.collect_children(Some(child), out);
            }
        }
    }

    fn append_children(&self, origin: Option<Id>, out: &mut String) {
        if let Some(children) = self.children.get(&origin) {
            for &child in children {
                let elem = &self.elems[&child];
                if !elem.deleted {
                    out.push(elem.ch);
                }
                self.append_children(Some(child), out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(doc: &mut Rga, s: &str) -> Vec<Op> {
        s.chars()
            .enumerate()
            .map(|(i, c)| doc.insert(i, c))
            .collect()
    }

    #[test]
    fn local_editing_reads_back() {
        let mut doc = Rga::new(1);
        type_str(&mut doc, "hello");
        assert_eq!(doc.text(), "hello");
        doc.delete(0); // remove 'h'
        assert_eq!(doc.text(), "ello");
        doc.insert(0, 'H');
        assert_eq!(doc.text(), "Hello");
    }

    #[test]
    fn concurrent_edits_converge() {
        // Two replicas start from the same shared prefix.
        let mut a = Rga::new(1);
        let ops: Vec<Op> = type_str(&mut a, "ab");
        let mut b = Rga::new(2);
        for op in &ops {
            b.apply(op.clone());
        }
        assert_eq!(a.text(), "ab");
        assert_eq!(b.text(), "ab");

        // Concurrently: A inserts 'X' at index 1, B inserts 'Y' at index 1.
        let op_a = a.insert(1, 'X');
        let op_b = b.insert(1, 'Y');

        // Exchange operations (delivered in opposite orders).
        b.apply(op_a);
        a.apply(op_b);

        // Both replicas converge to the identical string.
        assert_eq!(a.text(), b.text());
        // And it contains all characters.
        let text = a.text();
        assert!(text.contains('X') && text.contains('Y'));
        assert_eq!(text.len(), 4);
    }

    #[test]
    fn out_of_order_delivery_is_buffered_then_integrated() {
        let mut a = Rga::new(1);
        let o1 = a.insert(0, 'a'); // id (1,1)
        let o2 = a.insert(1, 'b'); // origin (1,1)

        // Deliver the second op to B *before* the first — it should buffer,
        // then integrate once its origin arrives.
        let mut b = Rga::new(2);
        b.apply(o2);
        assert_eq!(b.text(), "", "op with missing origin is buffered");
        b.apply(o1);
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn deletes_are_idempotent() {
        let mut doc = Rga::new(1);
        type_str(&mut doc, "xy");
        let del = doc.delete(0).unwrap();
        doc.apply(del.clone()); // re-applying the same delete is a no-op
        doc.apply(del);
        assert_eq!(doc.text(), "y");
    }
}
