//! Event model for the kernel-wide event bus.
//!
//! Every subsystem in Dadhichi communicates by publishing and subscribing to
//! [`Event`]s. Subsystems never call each other directly; this keeps the
//! microkernel decoupled and makes new capabilities pluggable at runtime.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// A topic namespaces events so subscribers can filter cheaply without
/// deserializing every payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Topic(pub String);

impl Topic {
    /// Create a topic from anything string-like.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The topic as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S: Into<String>> From<S> for Topic {
    fn from(s: S) -> Self {
        Topic(s.into())
    }
}

/// A single message on the event bus.
///
/// The `payload` is an opaque JSON value so that any subsystem — including
/// dynamically loaded WASM plugins — can emit and consume events without a
/// compile-time dependency on the emitter's types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Globally unique identifier for this event instance.
    pub id: Uuid,
    /// The topic this event was published on.
    pub topic: Topic,
    /// Milliseconds since the Unix epoch when the event was created.
    pub timestamp_ms: u128,
    /// Free-form, self-describing payload.
    pub payload: serde_json::Value,
    /// Optional correlation id linking related events (e.g. an agent run).
    pub correlation_id: Option<Uuid>,
}

impl Event {
    /// Build an event on `topic` carrying `payload`.
    pub fn new(topic: impl Into<Topic>, payload: serde_json::Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            topic: topic.into(),
            timestamp_ms: now_ms(),
            payload,
            correlation_id: None,
        }
    }

    /// Serialize a typed payload into an event, failing if it is not JSON-able.
    pub fn typed<T: Serialize>(
        topic: impl Into<Topic>,
        payload: &T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::new(topic, serde_json::to_value(payload)?))
    }

    /// Attach a correlation id, returning `self` for chaining.
    pub fn with_correlation(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Attempt to deserialize the payload into a concrete type.
    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
