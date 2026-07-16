//! Content-aware permission rules for the tool registry.
//!
//! The registry's [`GrantSet`](crate::GrantSet) and
//! [`ApprovalPolicy`](crate::ApprovalPolicy) decide on a tool's *capability
//! class* (may this caller run commands at all?). This module adds the finer
//! layer grok-build popularised: rules that decide on the call's **content** —
//! *this* command string, *this* path, *this* MCP tool — merged from every
//! config source and resolved by severity (`deny > ask > allow`).
//!
//! A [`RuleSet`] evaluates to a [`RuleAction`] that maps 1:1 onto the existing
//! [`PermissionMode`](crate::PermissionMode), so it slots in front of the mode
//! policy at the single choke point in
//! [`ToolRegistry::invoke`](crate::ToolRegistry::invoke) without disturbing it.
//! An empty rule set evaluates to `None`, so the registry behaves exactly as it
//! did before rules existed.

mod commands;
mod glob;
mod mode;
mod rule;

pub use commands::{is_dangerous, is_read_only_command, primary_command, split_segments};
pub use mode::SessionMode;
pub use rule::{Pattern, PermissionRule, RuleAction, RuleParseError, ToolClass};

use std::borrow::Cow;

/// A concrete tool call, presented to the rule engine for a verdict.
#[derive(Debug, Clone, Copy)]
pub struct ToolCall<'a> {
    /// The tool's registry name, e.g. `"terminal.run"`.
    pub name: &'a str,
    /// Its derived class.
    pub class: ToolClass,
    /// The arguments it was invoked with.
    pub args: &'a serde_json::Value,
}

impl<'a> ToolCall<'a> {
    /// Build a call, deriving the class from `name`.
    pub fn new(name: &'a str, args: &'a serde_json::Value) -> Self {
        Self {
            name,
            class: ToolClass::of(name),
            args,
        }
    }

    /// The string a [`Pattern`] matches against: the command for `Bash`, the
    /// path for `Read`/`Edit`/`Grep`, the URL for `WebFetch`, and the tool name
    /// itself for `Mcp`. `None` when there is nothing meaningful to match.
    pub fn subject(&self) -> Option<Cow<'a, str>> {
        match self.class {
            ToolClass::Bash => self.arg_str("command").map(Cow::Borrowed),
            ToolClass::Read | ToolClass::Edit | ToolClass::Grep => self
                .arg_str("path")
                .or_else(|| self.arg_str("root"))
                .or_else(|| self.arg_str("file"))
                .map(Cow::Borrowed),
            ToolClass::WebFetch => self
                .arg_str("url")
                .or_else(|| self.arg_str("uri"))
                .map(Cow::Borrowed),
            ToolClass::Mcp => Some(Cow::Borrowed(self.name)),
            ToolClass::WebSearch | ToolClass::Any | ToolClass::Other => None,
        }
    }

    fn arg_str(&self, key: &str) -> Option<&'a str> {
        self.args.get(key).and_then(|v| v.as_str())
    }

    /// For a `Bash` call, the individual chained segments (or a single element
    /// when the command can't be split safely). Empty for non-Bash calls.
    pub fn bash_segments(&self) -> Vec<String> {
        if self.class != ToolClass::Bash {
            return Vec::new();
        }
        let Some(command) = self.arg_str("command") else {
            return Vec::new();
        };
        split_segments(command).unwrap_or_else(|| vec![command.to_string()])
    }
}

/// Whether `call` is a built-in read-only operation that auto-approves without
/// prompting: a read/search tool, or a shell command whose every chained
/// segment is a recognised read-only command. A convenience layered on top of
/// the grant and rule checks — never a security boundary.
pub fn is_read_only_call(call: &ToolCall) -> bool {
    match call.class {
        ToolClass::Read | ToolClass::Grep => true,
        ToolClass::Bash => {
            let segments = call.bash_segments();
            !segments.is_empty() && segments.iter().all(|s| is_read_only_command(s))
        }
        _ => false,
    }
}

/// A merged set of permission rules from every config source. Order is
/// irrelevant — evaluation is by severity.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<PermissionRule>,
}

impl RuleSet {
    /// An empty rule set (matches nothing → the registry's prior behaviour).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one rule.
    pub fn push(&mut self, rule: PermissionRule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    /// Merge another set's rules in (used when layering config sources).
    pub fn extend(&mut self, other: RuleSet) -> &mut Self {
        self.rules.extend(other.rules);
        self
    }

    /// Parse and add a batch of string-form rules under one action. On a parse
    /// error the offending rule is skipped and the error returned in the `Vec`,
    /// so one bad line never discards the rest (config-load semantics).
    pub fn add_strings<I, S>(&mut self, action: RuleAction, rules: I) -> Vec<RuleParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut errors = Vec::new();
        for s in rules {
            match PermissionRule::parse(action, s.as_ref()) {
                Ok(rule) => self.rules.push(rule),
                Err(e) => errors.push(e),
            }
        }
        errors
    }

    /// Number of rules held.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Resolve a call to an action by severity: any matching `Deny` wins, else
    /// any matching `Ask`, else any matching `Allow`, else `None` (no opinion —
    /// the registry falls through to auto-approvals and the mode policy).
    ///
    /// `Deny` and `Ask` are checked against every Bash segment *and* the whole
    /// command; `Allow` is checked against the whole command only. This is the
    /// documented asymmetry: pair narrow `allow` rules with `deny` rules for the
    /// patterns you want blocked, so `Bash(git *)` can't wave through
    /// `git status && rm -rf /`.
    pub fn evaluate(&self, call: &ToolCall) -> Option<RuleAction> {
        let segments = call.bash_segments();
        let subject = call.subject();

        let mut found_ask = false;
        let mut found_allow = false;

        for rule in &self.rules {
            // Class gate: an `Any` rule matches every call; otherwise the
            // classes must be equal. `Any`/`Other` are never a call's class.
            if rule.tool != ToolClass::Any && rule.tool != call.class {
                continue;
            }

            let whole_only = rule.action == RuleAction::Allow;
            if rule_matches(rule, call, &segments, subject.as_deref(), whole_only) {
                match rule.action {
                    RuleAction::Deny => return Some(RuleAction::Deny),
                    RuleAction::Ask => found_ask = true,
                    RuleAction::Allow => found_allow = true,
                }
            }
        }

        if found_ask {
            Some(RuleAction::Ask)
        } else if found_allow {
            Some(RuleAction::Allow)
        } else {
            None
        }
    }
}

/// Whether `rule` matches `call`. A pattern-less rule matches any call of the
/// class. For a Bash `deny`/`ask` rule (`whole_only == false`) the pattern is
/// tested against every segment and the whole command; otherwise against the
/// single subject string.
fn rule_matches(
    rule: &PermissionRule,
    call: &ToolCall,
    segments: &[String],
    subject: Option<&str>,
    whole_only: bool,
) -> bool {
    let Some(pattern) = &rule.pattern else {
        return true; // class-only rule
    };

    if call.class == ToolClass::Bash && !whole_only {
        // Test each segment, then the whole command as a fallback.
        if segments.iter().any(|seg| pattern.matches(seg)) {
            return true;
        }
    }

    match subject {
        Some(s) => pattern.matches(s),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ruleset(rules: &[(RuleAction, &str)]) -> RuleSet {
        let mut set = RuleSet::new();
        for (action, s) in rules {
            let errs = set.add_strings(*action, [*s]);
            assert!(errs.is_empty(), "parse failed for {s}");
        }
        set
    }

    #[test]
    fn empty_set_has_no_opinion() {
        let set = RuleSet::new();
        let args = json!({ "command": "rm -rf /" });
        assert_eq!(set.evaluate(&ToolCall::new("terminal.run", &args)), None);
    }

    #[test]
    fn deny_beats_allow_regardless_of_order() {
        let set = ruleset(&[
            (RuleAction::Allow, "Bash(git *)"),
            (RuleAction::Deny, "Bash(rm -rf *)"),
        ]);
        let args = json!({ "command": "rm -rf /tmp/x" });
        assert_eq!(
            set.evaluate(&ToolCall::new("terminal.run", &args)),
            Some(RuleAction::Deny)
        );
    }

    #[test]
    fn deny_matches_a_chained_segment() {
        // allow git*, but a denied segment in a chain still rejects.
        let set = ruleset(&[
            (RuleAction::Allow, "Bash(git *)"),
            (RuleAction::Deny, "Bash(rm -rf *)"),
        ]);
        let args = json!({ "command": "git status && rm -rf /" });
        assert_eq!(
            set.evaluate(&ToolCall::new("terminal.run", &args)),
            Some(RuleAction::Deny)
        );
    }

    #[test]
    fn allow_matches_whole_command_only() {
        // Only an allow rule present; a chained rm is NOT covered by allow
        // (allow matches the whole string, which starts with "git "), but with
        // no deny rule the verdict is Allow — the caller must add deny rules.
        let set = ruleset(&[(RuleAction::Allow, "Bash(git *)")]);
        let args = json!({ "command": "git status && rm -rf /" });
        assert_eq!(
            set.evaluate(&ToolCall::new("terminal.run", &args)),
            Some(RuleAction::Allow)
        );
    }

    #[test]
    fn ask_rule_interrupts() {
        let set = ruleset(&[(RuleAction::Ask, "Edit")]);
        let args = json!({ "path": "src/main.rs" });
        assert_eq!(
            set.evaluate(&ToolCall::new("fs.write", &args)),
            Some(RuleAction::Ask)
        );
    }

    #[test]
    fn path_deny_scopes_to_class() {
        let set = ruleset(&[(RuleAction::Deny, "Read(secrets/**)")]);
        let denied = json!({ "path": "secrets/prod.env" });
        let ok = json!({ "path": "src/main.rs" });
        assert_eq!(
            set.evaluate(&ToolCall::new("fs.read", &denied)),
            Some(RuleAction::Deny)
        );
        assert_eq!(set.evaluate(&ToolCall::new("fs.read", &ok)), None);
    }

    #[test]
    fn mcp_rule_matches_server_glob() {
        let set = ruleset(&[(RuleAction::Deny, "MCPTool(github.*)")]);
        let args = json!({});
        assert_eq!(
            set.evaluate(&ToolCall::new("github.create_issue", &args)),
            Some(RuleAction::Deny)
        );
    }

    #[test]
    fn wildcard_rule_matches_everything() {
        let set = ruleset(&[(RuleAction::Deny, "*")]);
        let args = json!({ "path": "x" });
        assert_eq!(
            set.evaluate(&ToolCall::new("fs.read", &args)),
            Some(RuleAction::Deny)
        );
    }

    #[test]
    fn read_tools_are_read_only() {
        let args = json!({ "path": "src/main.rs" });
        assert!(is_read_only_call(&ToolCall::new("fs.read", &args)));
        assert!(is_read_only_call(&ToolCall::new("fs.grep", &args)));
        assert!(!is_read_only_call(&ToolCall::new("fs.write", &args)));
    }

    #[test]
    fn read_only_shell_needs_every_segment_read_only() {
        let ro = json!({ "command": "ls && git status" });
        let mixed = json!({ "command": "ls && rm -rf /" });
        assert!(is_read_only_call(&ToolCall::new("terminal.run", &ro)));
        assert!(!is_read_only_call(&ToolCall::new("terminal.run", &mixed)));
    }
}
