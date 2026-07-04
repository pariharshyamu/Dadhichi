//! The Agent Console: a bus subscriber that renders live agent activity.
//!
//! It listens on `agent.*` topics and prints a readable trace of the plan,
//! model usage, and status transitions. In the GUI this same event stream
//! drives the Agent Console panel; here it renders to stdout so the runtime is
//! observable headlessly.

use dadhichi_core::{EventBus, RecvError};

/// Spawn a task that mirrors agent events to stdout until the bus closes.
///
/// Returns the [`tokio::task::JoinHandle`] so the caller can await a clean
/// shutdown after the agent run completes.
pub fn spawn(bus: &EventBus) -> tokio::task::JoinHandle<()> {
    let mut sub = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(event) if event.topic.as_str().starts_with("agent.") => {
                    println!("  ┃ [{}] {}", event.topic.as_str(), compact(&event.payload));
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    println!("  ┃ (console lagged, dropped {n} events)");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Render a JSON payload as a compact single line.
fn compact(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}
