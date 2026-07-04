//! The command layer.
//!
//! Where the [event bus](crate::bus) is fire-and-forget fan-out, the command
//! layer is request/response: the UI (or an agent) dispatches a named command
//! and awaits a typed result. Handlers are registered by name, keeping the UI
//! decoupled from the services that fulfil its intents.

use crate::error::{KernelError, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A command is a named intent plus a JSON argument bag.
#[derive(Debug, Clone)]
pub struct Command {
    /// Dot-namespaced command id, e.g. `"editor.format"` or `"agent.run"`.
    pub name: String,
    /// Arguments for the command.
    pub args: serde_json::Value,
}

impl Command {
    /// Construct a command with a null argument bag.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: serde_json::Value::Null,
        }
    }

    /// Attach typed arguments, failing if they are not serializable.
    pub fn with_args<T: serde::Serialize>(
        mut self,
        args: &T,
    ) -> std::result::Result<Self, serde_json::Error> {
        self.args = serde_json::to_value(args)?;
        Ok(self)
    }
}

/// A handler that fulfils a single command name.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Execute the command, returning a JSON result on success.
    async fn handle(&self, command: Command) -> Result<serde_json::Value>;
}

#[async_trait]
impl<F, Fut> CommandHandler for F
where
    F: Fn(Command) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<serde_json::Value>> + Send,
{
    async fn handle(&self, command: Command) -> Result<serde_json::Value> {
        (self)(command).await
    }
}

/// Routes commands to their registered handlers.
#[derive(Clone, Default)]
pub struct CommandRegistry {
    handlers: Arc<RwLock<HashMap<String, Arc<dyn CommandHandler>>>>,
}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRegistry").finish_non_exhaustive()
    }
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` under command `name`, replacing any prior handler.
    pub async fn register(&self, name: impl Into<String>, handler: Arc<dyn CommandHandler>) {
        self.handlers.write().await.insert(name.into(), handler);
    }

    /// Dispatch `command`, awaiting its handler's result.
    pub async fn dispatch(&self, command: Command) -> Result<serde_json::Value> {
        let handler = {
            let guard = self.handlers.read().await;
            guard.get(&command.name).cloned()
        };
        match handler {
            Some(h) => h.handle(command).await,
            None => Err(KernelError::NoHandler(command.name)),
        }
    }

    /// List the names of all registered commands (useful for a command palette).
    pub async fn command_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.handlers.read().await.keys().cloned().collect();
        names.sort();
        names
    }
}
