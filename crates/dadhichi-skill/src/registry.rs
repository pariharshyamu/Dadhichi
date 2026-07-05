//! [`SkillRegistry`]: a catalogue of skills, resolvable by name.
//!
//! Mirrors the shape of [`ToolRegistry`](dadhichi_mcp::ToolRegistry): register
//! skills, `list` their specs for a palette, and `get` one by name to equip it.

use crate::skill::{Skill, SkillSpec};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A name-indexed catalogue of [`Skill`]s.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Arc<Skill>>,
}

impl SkillRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with the built-in skill library.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        for skill in crate::builtin::all() {
            registry.register(skill);
        }
        registry
    }

    /// Register `skill` under its name, replacing any existing entry.
    pub fn register(&mut self, skill: Skill) -> &mut Self {
        self.skills.insert(skill.name.clone(), Arc::new(skill));
        self
    }

    /// Builder-style registration.
    pub fn with(mut self, skill: Skill) -> Self {
        self.register(skill);
        self
    }

    /// Resolve a skill by name.
    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        self.skills.get(name).cloned()
    }

    /// Whether a skill with `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }

    /// The specs of every registered skill, sorted by name — the discovery
    /// surface for a command palette or a `skills/list` API.
    pub fn list(&self) -> Vec<SkillSpec> {
        self.skills.values().map(|s| s.spec()).collect()
    }

    /// The registered skill names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Skill;

    #[test]
    fn register_get_and_list() {
        let mut reg = SkillRegistry::new();
        reg.register(Skill::new("b", "second"))
            .register(Skill::new("a", "first"));

        assert_eq!(reg.len(), 2);
        assert!(reg.contains("a"));
        assert_eq!(reg.get("a").unwrap().description, "first");
        // Sorted by name.
        assert_eq!(reg.names(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(reg.list()[0].name, "a");
    }

    #[test]
    fn register_replaces_by_name() {
        let mut reg = SkillRegistry::new();
        reg.register(Skill::new("x", "old"));
        reg.register(Skill::new("x", "new"));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("x").unwrap().description, "new");
    }

    #[test]
    fn builtins_are_populated() {
        let reg = SkillRegistry::with_builtins();
        assert!(!reg.is_empty());
        assert!(reg.contains("explain"));
    }
}
