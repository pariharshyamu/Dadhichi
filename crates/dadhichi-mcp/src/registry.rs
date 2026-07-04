//! The tool registry: a permission-aware catalogue of every available tool.

use crate::tool::{Permission, Tool, ToolError, ToolResult, ToolSpec};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tool` under its declared name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.spec().name, tool);
        self
    }

    /// The specs of every registered tool — this is what an MCP `tools/list`
    /// response or a model's tool-choice prompt is built from.
    pub fn list(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.tools.values().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Invoke `name` with `args`, enforcing `grants` first.
    ///
    /// This is the single choke point where permission is checked, so no tool
    /// can be reached without passing through the security gate.
    pub async fn invoke(
        &self,
        name: &str,
        args: serde_json::Value,
        grants: &GrantSet,
    ) -> ToolResult {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::Execution(format!("unknown tool: {name}")))?;

        let spec = tool.spec();
        if let Some(missing) = grants.first_missing(&spec.permissions) {
            return Err(ToolError::PermissionDenied(missing.to_string()));
        }

        tool.invoke(args).await
    }
}
