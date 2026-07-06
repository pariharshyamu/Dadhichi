//! Human-in-the-loop tool approval.
//!
//! Some capabilities are too consequential to run unattended — writing files,
//! executing shell commands, reaching the network. This module lets a
//! [`ToolRegistry`](crate::ToolRegistry) gate those calls behind an
//! [`ApprovalPolicy`]: each [`Permission`](crate::Permission) is either
//! [`Allow`](PermissionMode::Allow)ed silently, [`Deny`](PermissionMode::Deny)ed
//! outright, or [`Interrupt`](PermissionMode::Interrupt)ed — paused so an
//! [`Approver`] (a TUI prompt, a policy engine, a test double) can decide.
//!
//! The gate sits at the single choke point where every tool call already passes
//! its permission check, so no tool can dodge it, and it is `async` so an
//! interactive approver can await a keystroke without blocking the render loop.

use crate::tool::Permission;
use async_trait::async_trait;
use std::collections::HashMap;

/// What to do when a tool requires a given permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Run the tool without asking. The default for every permission.
    #[default]
    Allow,
    /// Pause and ask an [`Approver`] before running.
    Interrupt,
    /// Refuse the call outright.
    Deny,
}

/// A per-permission approval policy. Any permission not mentioned defaults to
/// [`PermissionMode::Allow`], so an empty policy reproduces the pre-approval
/// behaviour exactly.
#[derive(Debug, Clone, Default)]
pub struct ApprovalPolicy {
    modes: HashMap<Permission, PermissionMode>,
}

impl ApprovalPolicy {
    /// An all-[`Allow`](PermissionMode::Allow) policy.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Set the mode for `permission`, returning `self` for chaining.
    pub fn with(mut self, permission: Permission, mode: PermissionMode) -> Self {
        self.modes.insert(permission, mode);
        self
    }

    /// Set (or replace) the mode for `permission` in place.
    pub fn set(&mut self, permission: Permission, mode: PermissionMode) {
        self.modes.insert(permission, mode);
    }

    /// The mode for `permission` (defaulting to [`Allow`](PermissionMode::Allow)).
    pub fn mode(&self, permission: Permission) -> PermissionMode {
        self.modes
            .get(&permission)
            .copied()
            .unwrap_or(PermissionMode::Allow)
    }

    /// The strictest mode required by any permission in `required`, with
    /// `Deny > Interrupt > Allow`. This is the single decision the registry
    /// acts on: deny if anything is denied, otherwise interrupt if anything
    /// needs approval, otherwise allow.
    pub fn decide(&self, required: &[Permission]) -> PermissionMode {
        let mut worst = PermissionMode::Allow;
        for perm in required {
            match self.mode(*perm) {
                PermissionMode::Deny => return PermissionMode::Deny,
                PermissionMode::Interrupt => worst = PermissionMode::Interrupt,
                PermissionMode::Allow => {}
            }
        }
        worst
    }

    /// The first permission in `required` whose mode is `mode` — used to name
    /// the specific capability a prompt or denial is about.
    pub fn first_with_mode(
        &self,
        required: &[Permission],
        mode: PermissionMode,
    ) -> Option<Permission> {
        required.iter().copied().find(|p| self.mode(*p) == mode)
    }
}

/// A request put to an [`Approver`] when a tool needs interrupting.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// The tool about to run, e.g. `"terminal.run"`.
    pub tool: String,
    /// The permission that triggered the interrupt, e.g. `RunCommands`.
    pub permission: Permission,
    /// The arguments the tool was called with, so the approver can show them.
    pub args: serde_json::Value,
}

impl ApprovalRequest {
    /// A short, secret-free one-line summary for a prompt, e.g.
    /// `terminal.run (run_commands): {"command":"ls"}`.
    pub fn summary(&self) -> String {
        let args = serde_json::to_string(&self.args).unwrap_or_default();
        let args = if args.len() > 120 {
            format!("{}…", &args[..120])
        } else {
            args
        };
        format!("{} ({}): {}", self.tool, self.permission, args)
    }
}

/// The verdict an [`Approver`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool.
    Approve,
    /// Refuse the tool.
    Deny,
}

/// Decides whether an interrupted tool call may proceed.
///
/// Implementations bridge to wherever the human (or policy) lives: the TUI
/// publishes a prompt and awaits a keystroke; a headless runner might auto-deny;
/// a test supplies a canned answer.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Decide whether `request` may run. Called while the tool call is
    /// suspended, so it is fine to await user input here.
    async fn approve(&self, request: &ApprovalRequest) -> Decision;
}

/// An approver that returns the same decision for every request — handy for
/// tests and for a headless "deny everything interactive" mode.
#[derive(Debug, Clone, Copy)]
pub struct FixedApprover(pub Decision);

#[async_trait]
impl Approver for FixedApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> Decision {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_allows_everything() {
        let policy = ApprovalPolicy::allow_all();
        assert_eq!(
            policy.decide(&[Permission::RunCommands, Permission::WriteWorkspace]),
            PermissionMode::Allow
        );
    }

    #[test]
    fn deny_beats_interrupt_beats_allow() {
        let policy = ApprovalPolicy::default()
            .with(Permission::RunCommands, PermissionMode::Interrupt)
            .with(Permission::Network, PermissionMode::Deny);
        // Interrupt alone.
        assert_eq!(
            policy.decide(&[Permission::RunCommands, Permission::ReadWorkspace]),
            PermissionMode::Interrupt
        );
        // Deny dominates.
        assert_eq!(
            policy.decide(&[Permission::RunCommands, Permission::Network]),
            PermissionMode::Deny
        );
    }

    #[test]
    fn first_with_mode_names_the_permission() {
        let policy =
            ApprovalPolicy::default().with(Permission::RunCommands, PermissionMode::Interrupt);
        assert_eq!(
            policy.first_with_mode(
                &[Permission::ReadWorkspace, Permission::RunCommands],
                PermissionMode::Interrupt
            ),
            Some(Permission::RunCommands)
        );
    }

    #[tokio::test]
    async fn fixed_approver_returns_its_decision() {
        let approver = FixedApprover(Decision::Deny);
        let req = ApprovalRequest {
            tool: "terminal.run".into(),
            permission: Permission::RunCommands,
            args: serde_json::json!({ "command": "rm -rf /" }),
        };
        assert_eq!(approver.approve(&req).await, Decision::Deny);
        assert!(req.summary().contains("terminal.run"));
    }
}
