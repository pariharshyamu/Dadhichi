//! # dadhichi-core
//!
//! The **microkernel** at the heart of the Dadhichi IDE.
//!
//! The kernel owns three cross-cutting primitives and nothing else:
//!
//! | Primitive | Role | Communication style |
//! |-----------|------|---------------------|
//! | [`EventBus`] | broadcast notifications | fire-and-forget fan-out |
//! | [`CommandRegistry`] | named request/response intents | await a typed result |
//! | [`ServiceRegistry`] | shared, long-lived capabilities | resolve by type |
//!
//! Every other subsystem — AI runtime, agents, workspace, LSP, plugins — is a
//! *service* that plugs into these seams. No subsystem depends on another
//! directly; they observe events and dispatch commands. This is what makes the
//! IDE modular and hot-pluggable.
//!
//! ```
//! use dadhichi_core::{Kernel, Event};
//!
//! # async fn demo() {
//! let kernel = Kernel::new();
//! let mut sub = kernel.bus().subscribe_topic("editor.saved");
//!
//! kernel.bus().publish(Event::new("editor.saved", serde_json::json!({ "path": "main.rs" })));
//!
//! let event = sub.recv().await.unwrap();
//! assert_eq!(event.topic.as_str(), "editor.saved");
//! # }
//! ```

pub mod bus;
pub mod command;
pub mod error;
pub mod event;
pub mod service;

pub use bus::{EventBus, RecvError, Subscription};
pub use command::{Command, CommandHandler, CommandRegistry};
pub use error::{KernelError, Result};
pub use event::{Event, Topic};
pub use service::{Service, ServiceRegistry};

/// The kernel bundles the three core primitives into one cheap-to-clone handle.
///
/// Cloning a `Kernel` clones three `Arc`-backed handles, so every subsystem can
/// own a copy and they all share the same buses and registries.
#[derive(Clone, Debug, Default)]
pub struct Kernel {
    bus: EventBus,
    commands: CommandRegistry,
    services: ServiceRegistry,
}

impl Kernel {
    /// Boot a fresh kernel with empty registries.
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// The command registry.
    pub fn commands(&self) -> &CommandRegistry {
        &self.commands
    }

    /// The service registry.
    pub fn services(&self) -> &ServiceRegistry {
        &self.services
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn event_bus_delivers_to_matching_topic() {
        let kernel = Kernel::new();
        let mut sub = kernel.bus().subscribe_topic("fs.changed");

        kernel
            .bus()
            .publish(Event::new("unrelated", serde_json::json!({})));
        kernel
            .bus()
            .publish(Event::new("fs.changed", serde_json::json!({ "n": 1 })));

        let event = sub.recv().await.unwrap();
        assert_eq!(event.topic.as_str(), "fs.changed");
        assert_eq!(event.payload["n"], 1);
    }

    #[tokio::test]
    async fn command_dispatch_hits_registered_handler() {
        let kernel = Kernel::new();
        kernel
            .commands()
            .register(
                "math.double",
                Arc::new(|cmd: Command| async move {
                    let n = cmd.args["n"].as_i64().unwrap_or(0);
                    Ok(serde_json::json!({ "result": n * 2 }))
                }),
            )
            .await;

        let out = kernel
            .commands()
            .dispatch(
                Command::new("math.double")
                    .with_args(&serde_json::json!({ "n": 21 }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(out["result"], 42);
    }

    #[tokio::test]
    async fn missing_command_is_an_error() {
        let kernel = Kernel::new();
        let err = kernel
            .commands()
            .dispatch(Command::new("does.not.exist"))
            .await
            .unwrap_err();
        assert!(matches!(err, KernelError::NoHandler(_)));
    }

    #[tokio::test]
    async fn services_resolve_by_type() {
        #[derive(Debug, PartialEq)]
        struct Counter(u32);

        let kernel = Kernel::new();
        kernel.services().register(Arc::new(Counter(7))).await;

        let got = kernel.services().get::<Counter>().await.unwrap();
        assert_eq!(*got, Counter(7));
    }
}
