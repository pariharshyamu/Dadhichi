//! # dadhichi-plugin
//!
//! The **plugin SDK**. Third-party extensions ship as sandboxed WASM modules;
//! this crate defines the host-side contract they implement — a declarative
//! [`Manifest`] plus the [`Plugin`] lifecycle trait — and the capability set a
//! plugin must request up front. The host grants capabilities explicitly, so a
//! plugin can only reach what its manifest declares and the user approved.
//!
//! The trait here is the *stable ABI boundary*: the WASM runtime marshals calls
//! across it, but native (in-process) plugins used in tests implement it
//! directly.

use dadhichi_core::Kernel;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A capability a plugin may request in its manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Subscribe to and publish kernel events.
    EventBus,
    /// Register commands in the command palette.
    Commands,
    /// Read workspace files.
    ReadFiles,
    /// Write workspace files.
    WriteFiles,
    /// Register new tools for agents.
    ProvideTools,
    /// Make network requests.
    Network,
}

/// Declarative metadata a plugin publishes to the marketplace and the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Unique plugin id, e.g. `"com.example.rust-analyzer-bridge"`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Semantic version string.
    pub version: String,
    /// Capabilities the plugin needs to function.
    pub capabilities: Vec<Capability>,
}

/// Errors from plugin activation or command handling.
#[derive(Debug, Error)]
pub enum PluginError {
    /// The plugin requested a capability the host refused to grant.
    #[error("capability not granted: {0:?}")]
    CapabilityDenied(Capability),
    /// Activation failed.
    #[error("activation failed: {0}")]
    Activation(String),
}

/// The host-side view of a loaded plugin.
///
/// The host calls [`activate`](Plugin::activate) once after granting the
/// manifest's capabilities, handing the plugin a [`Kernel`] handle through which
/// it observes events and registers commands. [`deactivate`](Plugin::deactivate)
/// is the symmetric teardown for hot-reload and uninstall.
#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin's manifest.
    fn manifest(&self) -> &Manifest;

    /// Called once when the plugin is enabled.
    async fn activate(&self, kernel: Kernel) -> Result<(), PluginError>;

    /// Called when the plugin is disabled or hot-reloaded. Default: no-op.
    async fn deactivate(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

/// The host's registry of installed plugins, enforcing capability grants at
/// load time.
#[derive(Default)]
pub struct PluginHost {
    granted: Vec<Capability>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("granted", &self.granted)
            .finish()
    }
}

impl PluginHost {
    /// Create a host with a fixed set of grantable capabilities.
    pub fn new(granted: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            granted: granted.into_iter().collect(),
        }
    }

    /// Verify every capability a plugin's manifest requests is granted, then
    /// activate it against `kernel`.
    pub async fn load(&self, plugin: &dyn Plugin, kernel: Kernel) -> Result<(), PluginError> {
        for cap in &plugin.manifest().capabilities {
            if !self.granted.contains(cap) {
                return Err(PluginError::CapabilityDenied(*cap));
            }
        }
        plugin.activate(kernel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_core::{Command, Kernel};
    use std::sync::Arc;

    struct HelloPlugin {
        manifest: Manifest,
    }

    #[async_trait::async_trait]
    impl Plugin for HelloPlugin {
        fn manifest(&self) -> &Manifest {
            &self.manifest
        }
        async fn activate(&self, kernel: Kernel) -> Result<(), PluginError> {
            kernel
                .commands()
                .register(
                    "hello.greet",
                    Arc::new(|_cmd: Command| async move { Ok(serde_json::json!("hi")) }),
                )
                .await;
            Ok(())
        }
    }

    fn hello(caps: Vec<Capability>) -> HelloPlugin {
        HelloPlugin {
            manifest: Manifest {
                id: "test.hello".into(),
                name: "Hello".into(),
                version: "0.1.0".into(),
                capabilities: caps,
            },
        }
    }

    #[tokio::test]
    async fn plugin_activates_and_registers_command() {
        let host = PluginHost::new([Capability::Commands]);
        let kernel = Kernel::new();
        host.load(&hello(vec![Capability::Commands]), kernel.clone())
            .await
            .unwrap();

        let out = kernel
            .commands()
            .dispatch(Command::new("hello.greet"))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!("hi"));
    }

    #[tokio::test]
    async fn ungranted_capability_is_rejected() {
        let host = PluginHost::new([Capability::Commands]);
        let err = host
            .load(&hello(vec![Capability::Network]), Kernel::new())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PluginError::CapabilityDenied(Capability::Network)
        ));
    }
}
