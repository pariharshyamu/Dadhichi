//! The [`Skill`] data model: a reusable, permission-scoped capability bundle.
//!
//! A skill is *not* a tool. A tool is a single JSON-in/JSON-out function; a
//! skill is a **named recipe** that combines an instruction prompt, the set of
//! permissions a run must hold, an allow-list of tools the skill may reach, and
//! a plan template. Equipping a skill therefore both *guides* an agent (the
//! instructions and steps) and *bounds* it (the permissions and tool scope).
//!
//! Skills are plain data — `Serialize`/`Deserialize` — so they can be authored
//! in code, loaded from a manifest file, or shipped through the extension
//! marketplace, all behind the same type.

use dadhichi_mcp::{GrantSet, Permission};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Which tools a skill is allowed to invoke.
///
/// This is the scope that makes a skill *more* than its permissions: even when
/// a run holds `WriteWorkspace`, a skill scoped to `Allow({"fs.read"})` cannot
/// reach `fs.write`. Tool access is the intersection of the run's grants and
/// the skill's allow-list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "names")]
pub enum SkillTools {
    /// No tools — a pure reasoning/prompt skill.
    #[default]
    None,
    /// Only the named tools may be used.
    Allow(BTreeSet<String>),
    /// Any registered tool may be used (still permission-gated at the registry).
    Any,
}

impl SkillTools {
    /// Allow exactly the named tools.
    pub fn allow<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        SkillTools::Allow(names.into_iter().map(Into::into).collect())
    }

    /// Whether `tool` is reachable under this scope.
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            SkillTools::None => false,
            SkillTools::Any => true,
            SkillTools::Allow(set) => set.contains(tool),
        }
    }

    /// The explicitly allowed tool names (empty for `None`/`Any`).
    pub fn names(&self) -> Vec<&str> {
        match self {
            SkillTools::Allow(set) => set.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

/// A single step in a skill's plan template.
///
/// A step is either reasoning (`tool` is `None`) or a concrete tool call
/// (`tool` names a capability, `args` are the JSON arguments passed to it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillStep {
    /// A short imperative description shown in the plan and console.
    pub description: String,
    /// The tool this step invokes, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Arguments passed to `tool`. Ignored for reasoning steps.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub args: serde_json::Value,
}

impl SkillStep {
    /// A reasoning-only step.
    pub fn think(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            tool: None,
            args: serde_json::Value::Null,
        }
    }

    /// A step that invokes `tool` with `args`.
    pub fn tool(
        description: impl Into<String>,
        tool: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            description: description.into(),
            tool: Some(tool.into()),
            args,
        }
    }
}

/// A reusable, permission-scoped capability bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// Unique, stable id, e.g. `"code-review"`.
    pub name: String,
    /// One-line human-readable description for palettes and `list`.
    pub description: String,
    /// The system-prompt fragment injected when the skill is equipped.
    #[serde(default)]
    pub instructions: String,
    /// Permissions a run must hold to use this skill.
    #[serde(default)]
    pub required_permissions: Vec<Permission>,
    /// The tools this skill may invoke.
    #[serde(default)]
    pub tools: SkillTools,
    /// The plan template executed when the skill runs.
    #[serde(default)]
    pub steps: Vec<SkillStep>,
    /// Optional model id this skill prefers (overrides the agent default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Skill {
    /// Start a skill with a name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            instructions: String::new(),
            required_permissions: Vec::new(),
            tools: SkillTools::None,
            steps: Vec::new(),
            model: None,
        }
    }

    /// Set the instruction prompt.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Require a permission (deduplicated).
    pub fn require(mut self, permission: Permission) -> Self {
        if !self.required_permissions.contains(&permission) {
            self.required_permissions.push(permission);
        }
        self
    }

    /// Scope the skill to exactly the named tools.
    pub fn allow_tools<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = SkillTools::allow(names);
        self
    }

    /// Allow the skill to reach any registered tool (still permission-gated).
    pub fn allow_any_tools(mut self) -> Self {
        self.tools = SkillTools::Any;
        self
    }

    /// Append a plan step.
    pub fn step(mut self, step: SkillStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Append a reasoning step (convenience over [`Skill::step`]).
    pub fn think(self, description: impl Into<String>) -> Self {
        self.step(SkillStep::think(description))
    }

    /// Prefer a specific model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// The grant set a run needs to satisfy this skill's requirements.
    pub fn required_grants(&self) -> GrantSet {
        self.required_permissions.iter().copied().collect()
    }

    /// Secret-free listing metadata, for palettes and `skills/list`-style APIs.
    pub fn spec(&self) -> SkillSpec {
        SkillSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            permissions: self.required_permissions.clone(),
            tools: self.tools.clone(),
            steps: self.steps.len(),
        }
    }

    /// Parse a skill from a JSON manifest.
    pub fn from_json(source: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(source)
    }

    /// Render this skill as a pretty JSON manifest.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Compact, serialisable description of a skill for discovery UIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillSpec {
    /// The skill id.
    pub name: String,
    /// The one-line description.
    pub description: String,
    /// Permissions the skill requires.
    pub permissions: Vec<Permission>,
    /// The tool scope.
    pub tools: SkillTools,
    /// Number of plan steps.
    pub steps: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_scope_semantics() {
        assert!(!SkillTools::None.allows("fs.read"));
        assert!(SkillTools::Any.allows("anything"));
        let scope = SkillTools::allow(["fs.read", "git.diff"]);
        assert!(scope.allows("fs.read"));
        assert!(!scope.allows("fs.write"));
    }

    #[test]
    fn builder_assembles_a_skill() {
        let skill = Skill::new("code-review", "Review a diff")
            .with_instructions("You are a meticulous reviewer.")
            .require(Permission::ReadWorkspace)
            .require(Permission::ReadWorkspace) // deduped
            .allow_tools(["fs.read"])
            .think("read the change")
            .step(SkillStep::tool(
                "scan",
                "fs.read",
                serde_json::json!({"path": "x"}),
            ));

        assert_eq!(skill.required_permissions, vec![Permission::ReadWorkspace]);
        assert!(skill.tools.allows("fs.read"));
        assert_eq!(skill.steps.len(), 2);
        assert!(skill.required_grants().allows(&[Permission::ReadWorkspace]));
    }

    #[test]
    fn spec_is_secret_free_summary() {
        let skill = Skill::new("audit", "Security audit")
            .require(Permission::ReadWorkspace)
            .allow_tools(["fs.read"])
            .think("scan");
        let spec = skill.spec();
        assert_eq!(spec.name, "audit");
        assert_eq!(spec.steps, 1);
        assert_eq!(spec.permissions, vec![Permission::ReadWorkspace]);
    }

    #[test]
    fn round_trips_through_json_manifest() {
        let skill = Skill::new("explain", "Explain code")
            .with_instructions("Explain clearly.")
            .allow_any_tools()
            .think("summarise");
        let json = skill.to_json();
        let parsed = Skill::from_json(&json).unwrap();
        assert_eq!(parsed, skill);
    }

    #[test]
    fn minimal_manifest_uses_defaults() {
        // Only name + description supplied; everything else defaults.
        let skill = Skill::from_json(r#"{"name":"x","description":"y"}"#).unwrap();
        assert_eq!(skill.tools, SkillTools::None);
        assert!(skill.required_permissions.is_empty());
        assert!(skill.steps.is_empty());
        assert!(skill.instructions.is_empty());
    }
}
