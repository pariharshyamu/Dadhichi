//! The pre-tool-use gate: a hook point that can veto a call before any
//! permission check.
//!
//! Grok-style `PreToolUse` hooks run *first* — a hook may deny a tool call
//! before grants, rules, or the mode policy are consulted; a hook that allows
//! simply declines to deny and lets the normal checks proceed. To keep the
//! `dadhichi-mcp` tool registry free of a dependency on the higher-level hooks
//! runtime, the registry depends only on this trait (the same inversion used
//! for [`Approver`](crate::Approver)); `dadhichi-hooks` provides the
//! implementation.

use async_trait::async_trait;

/// The verdict a [`PreToolUseGate`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// No hook objected — fall through to the normal permission checks.
    Proceed,
    /// A hook denied the call outright, with a reason for the transcript.
    Deny(String),
}

/// A gate consulted before a tool call is authorized. Implementations must be
/// **fail-open**: any internal error (a crashed hook, a timeout) should return
/// [`Proceed`](GateDecision::Proceed), never spuriously deny.
#[async_trait]
pub trait PreToolUseGate: Send + Sync {
    /// Decide whether the call to `tool` with `args` may proceed to the
    /// permission checks.
    async fn check(&self, tool: &str, args: &serde_json::Value) -> GateDecision;
}
