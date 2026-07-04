//! # dadhichi-collab
//!
//! **Live collaboration** primitives. Two people (or a person and an AI
//! participant) edit the same buffer with no central authority: the document is
//! a [`Rga`] sequence CRDT that converges under concurrent edits, and
//! [`Presence`] tracks shared cursors and who is in the room.
//!
//! ```
//! use dadhichi_collab::Rga;
//!
//! let mut a = Rga::new(1);
//! let mut b = Rga::new(2);
//! let ops: Vec<_> = "hi".chars().enumerate().map(|(i, c)| a.insert(i, c)).collect();
//! for op in ops { b.apply(op); }
//!
//! // Concurrent inserts still converge.
//! let oa = a.insert(2, '!');
//! let ob = b.insert(0, '>');
//! b.apply(oa);
//! a.apply(ob);
//! assert_eq!(a.text(), b.text());
//! ```

pub mod presence;
pub mod rga;

pub use presence::{Peer, Presence};
pub use rga::{Id, Op, Rga};
