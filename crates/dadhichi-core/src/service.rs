//! A type-indexed registry of long-lived core services.
//!
//! Services (workspace, indexer, AI runtime, …) register themselves once and
//! are resolved by type. This is the microkernel's dependency-injection seam:
//! a subsystem asks the kernel for a capability rather than constructing it.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A marker for anything that can live in the [`ServiceRegistry`].
///
/// The `Send + Sync + 'static` bound lets services be shared across the Tokio
/// runtime's worker threads.
pub trait Service: Any + Send + Sync + 'static {}

impl<T: Any + Send + Sync + 'static> Service for T {}

/// Stores at most one service instance per concrete type.
#[derive(Clone, Default)]
pub struct ServiceRegistry {
    services: Arc<RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRegistry").finish_non_exhaustive()
    }
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `service`, replacing any existing instance of the same type.
    pub async fn register<T: Service>(&self, service: Arc<T>) {
        self.services
            .write()
            .await
            .insert(TypeId::of::<T>(), service);
    }

    /// Resolve a previously registered service of type `T`.
    pub async fn get<T: Service>(&self) -> Option<Arc<T>> {
        self.services
            .read()
            .await
            .get(&TypeId::of::<T>())
            .and_then(|any| any.clone().downcast::<T>().ok())
    }

    /// Number of registered services.
    pub async fn len(&self) -> usize {
        self.services.read().await.len()
    }

    /// Whether the registry holds no services.
    pub async fn is_empty(&self) -> bool {
        self.services.read().await.is_empty()
    }
}
