//! Hook lifecycle events.

/// A point in a session's lifecycle at which hooks can fire.
///
/// Only [`PreToolUse`](HookEvent::PreToolUse) can block; every other event is
/// passive (its output is ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// A session starts.
    SessionStart,
    /// The user submits a prompt.
    UserPromptSubmit,
    /// A tool is about to run — the only event that can deny.
    PreToolUse,
    /// A tool completed successfully.
    PostToolUse,
    /// A tool failed.
    PostToolUseFailure,
    /// The permission system denied a tool call.
    PermissionDenied,
    /// An agent turn ended.
    Stop,
    /// A turn ended because of an API error.
    StopFailure,
    /// The agent sent a notification.
    Notification,
    /// A subagent started.
    SubagentStart,
    /// A subagent finished.
    SubagentStop,
    /// Conversation compaction is about to run.
    PreCompact,
    /// Conversation compaction completed.
    PostCompact,
    /// The session ends.
    SessionEnd,
}

impl HookEvent {
    /// Parse the event name used in a hook file. Accepts the canonical
    /// PascalCase names, plus `SubagentEnd` as an alias for `SubagentStop`.
    /// Unknown names return `None` (the caller skips them, so a shared Claude/
    /// Cursor settings file still loads).
    pub fn parse(s: &str) -> Option<HookEvent> {
        Some(match s {
            "SessionStart" => HookEvent::SessionStart,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "PreToolUse" => HookEvent::PreToolUse,
            "PostToolUse" => HookEvent::PostToolUse,
            "PostToolUseFailure" => HookEvent::PostToolUseFailure,
            "PermissionDenied" => HookEvent::PermissionDenied,
            "Stop" => HookEvent::Stop,
            "StopFailure" => HookEvent::StopFailure,
            "Notification" => HookEvent::Notification,
            "SubagentStart" => HookEvent::SubagentStart,
            "SubagentStop" | "SubagentEnd" => HookEvent::SubagentStop,
            "PreCompact" => HookEvent::PreCompact,
            "PostCompact" => HookEvent::PostCompact,
            "SessionEnd" => HookEvent::SessionEnd,
            _ => return None,
        })
    }

    /// The canonical event name (also the value of `DADHICHI_HOOK_EVENT`, but
    /// snake_cased — see [`as_env`](HookEvent::as_env)).
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PermissionDenied => "PermissionDenied",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::Notification => "Notification",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }

    /// The snake_case form used for `hookEventName` in the payload and the
    /// `DADHICHI_HOOK_EVENT` environment variable (`pre_tool_use`, …).
    pub fn as_env(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::PermissionDenied => "permission_denied",
            HookEvent::Stop => "stop",
            HookEvent::StopFailure => "stop_failure",
            HookEvent::Notification => "notification",
            HookEvent::SubagentStart => "subagent_start",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostCompact => "post_compact",
            HookEvent::SessionEnd => "session_end",
        }
    }

    /// Whether a hook on this event can block the action. Only `PreToolUse`.
    pub fn is_blocking(&self) -> bool {
        matches!(self, HookEvent::PreToolUse)
    }

    /// Whether a `matcher` (tested against the tool / notification name) is
    /// meaningful for this event.
    pub fn accepts_matcher(&self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionDenied
                | HookEvent::Notification
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trip_and_alias() {
        assert_eq!(HookEvent::parse("PreToolUse"), Some(HookEvent::PreToolUse));
        assert_eq!(
            HookEvent::parse("SubagentEnd"),
            Some(HookEvent::SubagentStop)
        );
        assert_eq!(HookEvent::parse("Nope"), None);
        assert_eq!(HookEvent::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookEvent::PreToolUse.as_env(), "pre_tool_use");
    }

    #[test]
    fn only_pre_tool_use_blocks() {
        assert!(HookEvent::PreToolUse.is_blocking());
        assert!(!HookEvent::PostToolUse.is_blocking());
        assert!(!HookEvent::SessionStart.is_blocking());
    }
}
