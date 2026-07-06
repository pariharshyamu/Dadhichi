//! The tool registry: a permission-aware catalogue of every available tool.

use crate::approval::{ApprovalPolicy, ApprovalRequest, Approver, Decision, PermissionMode};
use crate::tool::{Permission, Tool, ToolError, ToolResult, ToolSpec};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// A set of permissions granted to a caller (an agent, a plugin, the user).
#[derive(Debug, Clone, Default)]
pub struct GrantSet {
    granted: HashSet<Permission>,
}

impl GrantSet {
    /// An empty grant set (no permissions).
    pub fn none() -> Self {
        Self::default()
    }

    /// Grant a permission.
    pub fn grant(&mut self, perm: Permission) -> &mut Self {
        self.granted.insert(perm);
        self
    }

    /// Whether every permission in `required` is held.
    pub fn allows(&self, required: &[Permission]) -> bool {
        required.iter().all(|p| self.granted.contains(p))
    }

    /// The first required permission that is missing, if any.
    pub fn first_missing(&self, required: &[Permission]) -> Option<Permission> {
        required.iter().copied().find(|p| !self.granted.contains(p))
    }
}

impl FromIterator<Permission> for GrantSet {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        Self {
            granted: iter.into_iter().collect(),
        }
    }
}

/// Catalogues tools and enforces permissions at the call boundary.
///
/// The catalogue is **interior-mutable** (an `RwLock`), so tools can be
/// registered or removed at runtime through a shared `Arc<ToolRegistry>` — this
/// is what lets MCP connectors bridge in a server's tools after boot without
/// rebuilding the registry every agent already holds.
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    /// Per-permission approval policy (default: allow everything).
    policy: RwLock<ApprovalPolicy>,
    /// The approver consulted when a call is interrupted. Without one, an
    /// `Interrupt` mode falls back to allowing the call (headless default).
    approver: RwLock<Option<Arc<dyn Approver>>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tool` under its declared name (replacing any existing tool of
    /// that name). Takes `&self`, so tools can be added through a shared handle.
    pub fn register(&self, tool: Arc<dyn Tool>) -> &Self {
        let name = tool.spec().name;
        self.write().insert(name, tool);
        self
    }

    /// Remove the tool named `name`, returning whether one was present.
    pub fn unregister(&self, name: &str) -> bool {
        self.write().remove(name).is_some()
    }

    /// Whether a tool named `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.read().contains_key(name)
    }

    /// The registered tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// The specs of every registered tool — this is what an MCP `tools/list`
    /// response or a model's tool-choice prompt is built from.
    pub fn list(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.read().values().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Replace the approval policy that gates interrupting/denied permissions.
    pub fn set_policy(&self, policy: ApprovalPolicy) -> &Self {
        *self.policy.write().unwrap_or_else(|e| e.into_inner()) = policy;
        self
    }

    /// The current approval policy.
    pub fn policy(&self) -> ApprovalPolicy {
        self.policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Install the [`Approver`] consulted when a call is interrupted (a TUI
    /// prompt, a policy engine, a test double). Replaces any previous one.
    pub fn set_approver(&self, approver: Arc<dyn Approver>) -> &Self {
        *self.approver.write().unwrap_or_else(|e| e.into_inner()) = Some(approver);
        self
    }

    /// Invoke `name` with `args`, enforcing `grants` and the approval policy.
    ///
    /// This is the single choke point where capability is checked, so no tool
    /// can be reached without passing through both gates: first the static
    /// `grants` (does the caller hold the permission at all?), then the dynamic
    /// [`ApprovalPolicy`] (should this specific call be denied, or paused for a
    /// human?).
    pub async fn invoke(
        &self,
        name: &str,
        args: serde_json::Value,
        grants: &GrantSet,
    ) -> ToolResult {
        // Resolve and clone the tool handle, then drop the lock before awaiting.
        let tool = self
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::Execution(format!("unknown tool: {name}")))?;

        let spec = tool.spec();
        if let Some(missing) = grants.first_missing(&spec.permissions) {
            return Err(ToolError::PermissionDenied(missing.to_string()));
        }

        // Dynamic approval gate: deny outright, or interrupt for a human verdict.
        let policy = self.policy();
        match policy.decide(&spec.permissions) {
            PermissionMode::Allow => {}
            PermissionMode::Deny => {
                let perm = policy
                    .first_with_mode(&spec.permissions, PermissionMode::Deny)
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                return Err(ToolError::Rejected(format!(
                    "{name} denied by policy ({perm})"
                )));
            }
            PermissionMode::Interrupt => {
                let approver = self
                    .approver
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(approver) = approver {
                    let permission = policy
                        .first_with_mode(&spec.permissions, PermissionMode::Interrupt)
                        .unwrap_or(Permission::RunCommands);
                    let request = ApprovalRequest {
                        tool: name.to_string(),
                        permission,
                        args: args.clone(),
                    };
                    if approver.approve(&request).await == Decision::Deny {
                        return Err(ToolError::Rejected(format!("{name} rejected by reviewer")));
                    }
                }
                // No approver wired ⇒ nothing can answer the interrupt, so fall
                // through and allow (a headless run isn't blocked by a prompt).
            }
        }

        tool.invoke(args).await
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.tools.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.tools.write().unwrap_or_else(|e| e.into_inner())
    }
}
