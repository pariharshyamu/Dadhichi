//! Planning primitives: an agent decomposes a goal into ordered [`Step`]s.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single unit of work in a [`Plan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Stable id for referencing this step in checkpoints and the console.
    pub id: Uuid,
    /// A short imperative description, e.g. "run the test suite".
    pub description: String,
    /// The name of the tool this step will invoke, if it uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Whether the step has completed.
    pub done: bool,
}

impl Step {
    /// A reasoning-only step (no tool call).
    pub fn think(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            tool: None,
            done: false,
        }
    }

    /// A step that invokes `tool`.
    pub fn using(description: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            tool: Some(tool.into()),
            done: false,
        }
    }
}

/// An ordered, mutable plan an agent executes and reflects on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The goal this plan aims to satisfy.
    pub goal: String,
    /// The ordered steps.
    pub steps: Vec<Step>,
}

impl Plan {
    /// Start a plan for `goal`.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            steps: Vec::new(),
        }
    }

    /// Append a step, returning `self` for chaining.
    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// The next not-yet-done step, if any.
    pub fn next_pending(&self) -> Option<&Step> {
        self.steps.iter().find(|s| !s.done)
    }

    /// Mark the step with `id` complete. Returns `true` if it was found.
    pub fn complete(&mut self, id: Uuid) -> bool {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.done = true;
            true
        } else {
            false
        }
    }

    /// Fraction of steps completed in `0.0..=1.0` (empty plans report `1.0`).
    pub fn progress(&self) -> f32 {
        if self.steps.is_empty() {
            return 1.0;
        }
        let done = self.steps.iter().filter(|s| s.done).count();
        done as f32 / self.steps.len() as f32
    }
}
