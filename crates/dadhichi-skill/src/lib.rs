//! # dadhichi-skill
//!
//! **Skills** — reusable, permission-scoped capability bundles that an agent
//! equips to pursue a goal.
//!
//! A [tool](dadhichi_mcp::Tool) is a single function; a [`Skill`] is a *recipe*
//! that combines four things:
//!
//! 1. **Instructions** — a system-prompt fragment that steers the model.
//! 2. **Required permissions** — the [`Permission`](dadhichi_mcp::Permission)s a
//!    run must hold to use the skill at all.
//! 3. **A tool scope** — an allow-list ([`SkillTools`]) that bounds which tools
//!    the skill may reach, independently of (and narrower than) the run's grants.
//! 4. **A plan template** — the [`SkillStep`]s executed when the skill runs.
//!
//! The [`SkillAgent`] runs a skill as a first-class
//! [`Agent`](dadhichi_agent::Agent): it enforces the required permissions,
//! executes tool steps through a [`ScopedTools`] gate (so a skill can be
//! *strictly less* capable than the grants the run carries), consults the
//! model under the skill's instructions, and reflects — emitting `skill.*`
//! events the whole way.
//!
//! ```
//! use dadhichi_skill::{Skill, SkillAgent, SkillRegistry};
//! use dadhichi_mcp::Permission;
//!
//! // Author a skill (or load one from a JSON manifest, or use a built-in).
//! let review = Skill::new("code-review", "Review a change")
//!     .with_instructions("You are a meticulous reviewer.")
//!     .require(Permission::ReadWorkspace)
//!     .allow_tools(["fs.read", "git.diff"]);
//!
//! let mut registry = SkillRegistry::with_builtins();
//! registry.register(review);
//! assert!(registry.contains("code-review"));
//!
//! // Equip it as an agent the orchestrator can run.
//! let agent = SkillAgent::new(registry.get("code-review").unwrap());
//! assert_eq!(agent.name(), "skill:code-review");
//! # use dadhichi_agent::Agent;
//! ```

pub mod agent;
pub mod builtin;
pub mod loader;
pub mod registry;
pub mod scope;
pub mod skill;
pub mod watcher;

pub use agent::SkillAgent;
pub use loader::{LoadError, LoadReport, skill_dirs};
pub use registry::SkillRegistry;
pub use scope::{ScopedTools, SkillError};
pub use skill::{Skill, SkillSpec, SkillStep, SkillTools};
pub use watcher::{SharedSkills, SkillWatchError, SkillWatchGuard, shared, watch_skills};

// Re-exported for convenience so callers can build skills without also
// importing `dadhichi-mcp` directly.
pub use dadhichi_mcp::Permission;
