//! Error types shared across the Dadhichi kernel.

use thiserror::Error;

/// The canonical result type used throughout the kernel.
pub type Result<T> = std::result::Result<T, KernelError>;

/// Errors that can arise inside the microkernel and its core services.
#[derive(Debug, Error)]
pub enum KernelError {
    /// A service was requested from the registry but was never registered.
    #[error("service not found: {0}")]
    ServiceNotFound(String),

    /// A command was dispatched but no handler is registered for it.
    #[error("no handler registered for command: {0}")]
    NoHandler(String),

    /// A command handler returned a domain error.
    #[error("command failed: {0}")]
    CommandFailed(String),

    /// The event bus has been shut down and can no longer accept traffic.
    #[error("event bus is closed")]
    BusClosed,

    /// A (de)serialization step failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Any other error, type-erased for convenience at boundaries.
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl KernelError {
    /// Convenience constructor for a command failure from any displayable value.
    pub fn command_failed(msg: impl std::fmt::Display) -> Self {
        Self::CommandFailed(msg.to_string())
    }
}
