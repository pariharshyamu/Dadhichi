//! [`ScopedTools`]: the choke point that enforces a skill's tool scope.
//!
//! The tool registry already gates every call on the caller's [`GrantSet`].
//! `ScopedTools` layers a second, narrower gate on top: the skill's tool
//! allow-list. A call must pass *both* — the skill must permit the tool name
//! *and* the run must hold the permissions the tool declares — before it
//! reaches the underlying [`ToolRegistry`]. That is what lets one skill be
//! strictly less capable than the grants a run happens to carry.

use crate::skill::SkillTools;
use dadhichi_mcp::{GrantSet, ToolError, ToolRegistry, ToolSpec};
use thiserror::Error;

/// Why a scoped tool call was refused.
#[derive(Debug, Error)]
pub enum SkillError {
    /// The tool is not in the skill's allow-list.
    #[error("tool '{0}' is not in this skill's allowed set")]
    ToolNotAllowed(String),
    /// The underlying registry rejected or failed the call (including a
    /// permission denial from the run's grants).
    #[error(transparent)]
    Tool(#[from] ToolError),
}

/// A capability-scoped view over a shared [`ToolRegistry`].
///
/// Borrows the registry, the skill's [`SkillTools`] scope, and the run's
/// grants; every [`ScopedTools::invoke`] is checked against the scope first and
/// the grants second.
#[derive(Debug)]
pub struct ScopedTools<'a> {
    registry: &'a ToolRegistry,
    scope: &'a SkillTools,
    grants: &'a GrantSet,
}

impl<'a> ScopedTools<'a> {
    /// Wrap `registry`, restricting it to `scope` under `grants`.
    pub fn new(registry: &'a ToolRegistry, scope: &'a SkillTools, grants: &'a GrantSet) -> Self {
        Self {
            registry,
            scope,
            grants,
        }
    }

    /// Whether `tool` may be invoked through this scope.
    pub fn allows(&self, tool: &str) -> bool {
        self.scope.allows(tool)
    }

    /// The specs of the tools reachable through this scope — the menu an agent
    /// (or a model's tool-choice prompt) is allowed to pick from.
    pub fn list(&self) -> Vec<ToolSpec> {
        self.registry
            .list()
            .into_iter()
            .filter(|spec| self.scope.allows(&spec.name))
            .collect()
    }

    /// Invoke `name` with `args`, enforcing the skill scope then the grants.
    pub async fn invoke(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, SkillError> {
        if !self.scope.allows(name) {
            return Err(SkillError::ToolNotAllowed(name.to_string()));
        }
        Ok(self.registry.invoke(name, args, self.grants).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_mcp::{EchoTool, Permission, Tool, ToolResult, ToolSpec};
    use std::sync::Arc;

    /// A tool that requires `WriteWorkspace`, to exercise the grant gate.
    #[derive(Debug)]
    struct WriteTool;

    #[async_trait::async_trait]
    impl Tool for WriteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fs.write".into(),
                description: "write a file".into(),
                input_schema: serde_json::json!({}),
                permissions: vec![Permission::WriteWorkspace],
            }
        }
        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            Ok(serde_json::json!({"written": true}))
        }
    }

    fn registry() -> ToolRegistry {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        r.register(Arc::new(WriteTool));
        r
    }

    #[tokio::test]
    async fn allowed_tool_runs() {
        let reg = registry();
        let scope = SkillTools::allow(["echo"]);
        let grants = GrantSet::none();
        let scoped = ScopedTools::new(&reg, &scope, &grants);
        let out = scoped
            .invoke("echo", serde_json::json!({"v": 1}))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({"v": 1}));
    }

    #[tokio::test]
    async fn scope_blocks_tool_even_with_the_grant() {
        // The run *does* hold WriteWorkspace, but the skill scope excludes the
        // tool — the scope gate wins.
        let reg = registry();
        let scope = SkillTools::allow(["echo"]);
        let grants: GrantSet = [Permission::WriteWorkspace].into_iter().collect();
        let scoped = ScopedTools::new(&reg, &scope, &grants);
        let err = scoped
            .invoke("fs.write", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::ToolNotAllowed(_)));
    }

    #[tokio::test]
    async fn grant_gate_still_applies_within_scope() {
        // The skill allows the tool, but the run lacks the permission it needs.
        let reg = registry();
        let scope = SkillTools::allow(["fs.write"]);
        let grants = GrantSet::none();
        let scoped = ScopedTools::new(&reg, &scope, &grants);
        let err = scoped
            .invoke("fs.write", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SkillError::Tool(ToolError::PermissionDenied(_))
        ));
    }

    #[tokio::test]
    async fn list_shows_only_scoped_tools() {
        let reg = registry();
        let scope = SkillTools::allow(["echo"]);
        let grants = GrantSet::none();
        let scoped = ScopedTools::new(&reg, &scope, &grants);
        let names: Vec<_> = scoped.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["echo"]);
    }
}
