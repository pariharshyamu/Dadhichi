//! The event bus: a broadcast fan-out with optional per-topic filtering.
//!
//! Built on [`tokio::sync::broadcast`] so that any number of subscribers can
//! observe the stream concurrently without blocking the publisher. Subscribers
//! that fall behind observe [`RecvError::Lagged`] rather than stalling the
//! whole system — back-pressure is intentionally decoupled from correctness.

use crate::event::{Event, Topic};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Default channel capacity. Tuned to absorb bursts (e.g. a save-all touching
/// hundreds of files) without allocating unboundedly.
const DEFAULT_CAPACITY: usize = 4096;

/// A cloneable handle to the shared event bus.
///
/// Cloning is cheap (an `Arc` bump) and every clone talks to the same
/// underlying channel, so services can each hold their own handle.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<broadcast::Sender<Event>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.inner.receiver_count())
            .finish()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl EventBus {
    /// Create a bus with the default capacity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a bus with an explicit ring-buffer capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(tx),
        }
    }

    /// Publish `event` to every current subscriber.
    ///
    /// Returns the number of subscribers that received it. A return of `0` is
    /// not an error — it simply means nobody is listening yet.
    pub fn publish(&self, event: Event) -> usize {
        self.inner.send(event).unwrap_or(0)
    }

    /// Subscribe to *all* events. The returned [`Subscription`] yields events
    /// asynchronously until the bus is dropped.
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            rx: self.inner.subscribe(),
            filter: None,
        }
    }

    /// Subscribe but only observe events published on `topic`.
    pub fn subscribe_topic(&self, topic: impl Into<Topic>) -> Subscription {
        Subscription {
            rx: self.inner.subscribe(),
            filter: Some(topic.into()),
        }
    }

    /// Number of live subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.inner.receiver_count()
    }
}

/// A live subscription to the event bus.
#[derive(Debug)]
pub struct Subscription {
    rx: broadcast::Receiver<Event>,
    filter: Option<Topic>,
}

/// Why a `recv` ended without yielding a matching event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The bus has been dropped; no further events will ever arrive.
    Closed,
    /// The subscriber fell behind and `skipped` events were dropped for it.
    Lagged(u64),
}

impl Subscription {
    /// Await the next event matching this subscription's filter.
    ///
    /// Non-matching events are transparently skipped. `Lagged` is surfaced once
    /// so the caller can decide whether to re-sync state, then normal delivery
    /// resumes.
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        loop {
            match self.rx.recv().await {
                Ok(event) => {
                    if self.matches(&event) {
                        return Ok(event);
                    }
                    // Not our topic — keep waiting.
                }
                Err(broadcast::error::RecvError::Closed) => return Err(RecvError::Closed),
                Err(broadcast::error::RecvError::Lagged(n)) => return Err(RecvError::Lagged(n)),
            }
        }
    }

    fn matches(&self, event: &Event) -> bool {
        match &self.filter {
            Some(topic) => &event.topic == topic,
            None => true,
        }
    }
}
