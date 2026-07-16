//! Session permission modes.
//!
//! A [`SessionMode`] is the session-wide prompt policy consulted for a call
//! that matched no rule and isn't a built-in read-only auto-approval — grok's
//! `default` / `dontAsk` / `acceptEdits` / `bypassPermissions`. It is the *last*
//! step of resolution, so explicit rules and a policy `Deny` always win over it.

use crate::approval::PermissionMode;

/// The session-wide fall-through policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    /// Prompt for anything not pre-approved. The registry's default, and
    /// equivalent to the pre-modes behaviour.
    #[default]
    Default,
    /// Deny anything without an explicit allow rule or a built-in auto-approval.
    /// The correct default for headless / CI runs.
    DontAsk,
    /// Auto-approve file edits; prompt for the rest as in `Default`.
    AcceptEdits,
    /// Auto-approve calls. `deny` rules, hooks, and shell `ask` rules still
    /// apply (they resolve earlier); a policy `Deny` is still honoured.
    Bypass,
}

impl SessionMode {
    /// Resolve the fall-through verdict for a call that matched no rule.
    ///
    /// - `base` is the per-permission [`ApprovalPolicy`](crate::ApprovalPolicy)
    ///   decision. A `Deny` there always wins (deny is sticky).
    /// - `is_read_only` marks a built-in read-only auto-approval, which runs in
    ///   every mode (including `DontAsk`) unless denied above.
    /// - `is_edit` marks a file-mutating call, for `AcceptEdits`.
    pub fn resolve(self, base: PermissionMode, is_read_only: bool, is_edit: bool) -> PermissionMode {
        if base == PermissionMode::Deny {
            return PermissionMode::Deny;
        }
        if is_read_only {
            return PermissionMode::Allow;
        }
        match self {
            SessionMode::Default => base,
            SessionMode::DontAsk => PermissionMode::Deny,
            SessionMode::AcceptEdits => {
                if is_edit {
                    PermissionMode::Allow
                } else {
                    base
                }
            }
            SessionMode::Bypass => PermissionMode::Allow,
        }
    }

    /// Parse the mode name used on the CLI / in config.
    pub fn parse(s: &str) -> Option<SessionMode> {
        Some(match s {
            "default" => SessionMode::Default,
            "dontAsk" | "dont-ask" => SessionMode::DontAsk,
            "acceptEdits" | "accept-edits" => SessionMode::AcceptEdits,
            "bypassPermissions" | "bypass" => SessionMode::Bypass,
            _ => return None,
        })
    }

    /// The canonical mode name.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionMode::Default => "default",
            SessionMode::DontAsk => "dontAsk",
            SessionMode::AcceptEdits => "acceptEdits",
            SessionMode::Bypass => "bypassPermissions",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionMode::{Allow, Deny, Interrupt};

    #[test]
    fn policy_deny_is_sticky_in_every_mode() {
        for mode in [
            SessionMode::Default,
            SessionMode::DontAsk,
            SessionMode::AcceptEdits,
            SessionMode::Bypass,
        ] {
            assert_eq!(mode.resolve(Deny, false, false), Deny);
            // Even a read-only op can't override an explicit policy deny.
            assert_eq!(mode.resolve(Deny, true, false), Deny);
        }
    }

    #[test]
    fn read_only_auto_approves_even_under_dont_ask() {
        assert_eq!(SessionMode::DontAsk.resolve(Interrupt, true, false), Allow);
    }

    #[test]
    fn default_passes_the_base_through() {
        assert_eq!(SessionMode::Default.resolve(Interrupt, false, false), Interrupt);
        assert_eq!(SessionMode::Default.resolve(Allow, false, false), Allow);
    }

    #[test]
    fn dont_ask_denies_unmatched() {
        assert_eq!(SessionMode::DontAsk.resolve(Allow, false, false), Deny);
        assert_eq!(SessionMode::DontAsk.resolve(Interrupt, false, false), Deny);
    }

    #[test]
    fn accept_edits_allows_edits_only() {
        assert_eq!(SessionMode::AcceptEdits.resolve(Interrupt, false, true), Allow);
        assert_eq!(
            SessionMode::AcceptEdits.resolve(Interrupt, false, false),
            Interrupt
        );
    }

    #[test]
    fn bypass_allows_unmatched() {
        assert_eq!(SessionMode::Bypass.resolve(Interrupt, false, false), Allow);
    }

    #[test]
    fn parse_round_trip() {
        for m in [
            SessionMode::Default,
            SessionMode::DontAsk,
            SessionMode::AcceptEdits,
            SessionMode::Bypass,
        ] {
            assert_eq!(SessionMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(SessionMode::parse("nonsense"), None);
    }
}
