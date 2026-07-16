//! Permission rules: an action, the tool class it targets, and an optional
//! content pattern.
//!
//! Rules are parsed from the string form used on the CLI and in config
//! (`Bash(git *)`, `Read(src/**)`, `MCPTool(github.*)`, `WebFetch(domain:x.com)`,
//! or a bare class like `Read`, or `*` for everything). Their verdict —
//! [`RuleAction`] — maps 1:1 onto the registry's existing
//! [`PermissionMode`](crate::PermissionMode), so the rule layer sits cleanly in
//! front of the mode policy without changing anything downstream.

use super::glob::glob_match;
use thiserror::Error;

/// The verdict a matching rule carries. Ordered by severity: `Deny > Ask > Allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// Approve the call without prompting.
    Allow,
    /// Pause and ask the human/approver.
    Ask,
    /// Refuse the call outright.
    Deny,
}

/// The tool families a rule can target. A call's class is derived from its
/// registry name by [`ToolClass::of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Shell command execution (`terminal.run`).
    Bash,
    /// Reading / listing files (`fs.read`, `fs.ls`).
    Read,
    /// Mutating files (`fs.write`).
    Edit,
    /// Content search (`fs.grep`, `fs.glob`).
    Grep,
    /// A tool bridged in from an external MCP server (namespaced `server.tool`).
    Mcp,
    /// Outbound page fetches.
    WebFetch,
    /// Web search.
    WebSearch,
    /// A rule wildcard that matches every class. Never the class *of* a call.
    Any,
    /// A recognised tool that maps to none of the classes above (e.g. `echo`).
    /// Only an [`Any`](ToolClass::Any) rule matches it.
    Other,
}

impl ToolClass {
    /// Classify a registered tool by its name. Built-ins map to their class;
    /// a namespaced name (`server.tool` / `server__tool`) that isn't a known
    /// built-in is treated as an MCP tool; anything else is [`Other`](ToolClass::Other).
    pub fn of(tool_name: &str) -> ToolClass {
        match tool_name {
            "terminal.run" => ToolClass::Bash,
            "fs.read" | "fs.ls" => ToolClass::Read,
            "fs.write" => ToolClass::Edit,
            "fs.grep" | "fs.glob" => ToolClass::Grep,
            "web.fetch" | "webfetch" => ToolClass::WebFetch,
            "web.search" | "websearch" => ToolClass::WebSearch,
            name if name.contains("__") || name.contains('.') => ToolClass::Mcp,
            _ => ToolClass::Other,
        }
    }

    /// Parse the tool-name token used in a rule string (`Bash`, `Read`, …).
    fn parse(token: &str) -> Option<ToolClass> {
        Some(match token {
            "Bash" => ToolClass::Bash,
            "Read" | "NotebookRead" => ToolClass::Read,
            "Edit" | "Write" | "NotebookEdit" => ToolClass::Edit,
            "Grep" | "Glob" => ToolClass::Grep,
            "MCPTool" => ToolClass::Mcp,
            "WebFetch" => ToolClass::WebFetch,
            "WebSearch" => ToolClass::WebSearch,
            "*" => ToolClass::Any,
            _ => return None,
        })
    }

    /// Whether patterns for this class match with slash-crossing `*` (commands
    /// and names) rather than segment-bounded `*` (paths).
    fn cross_slash(self) -> bool {
        !matches!(self, ToolClass::Read | ToolClass::Edit | ToolClass::Grep)
    }
}

/// A content matcher for a rule.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// The subject must start with this text (character for character).
    Prefix(String),
    /// The subject must match this glob. `cross_slash` selects command/name vs
    /// path semantics (see [`glob_match`]).
    Glob { pattern: String, cross_slash: bool },
    /// A `WebFetch(domain:host)` rule: matches `host` and any subdomain,
    /// case-insensitively, ignoring a leading `www.`.
    Domain(String),
}

impl Pattern {
    /// Whether `subject` matches this pattern.
    pub fn matches(&self, subject: &str) -> bool {
        match self {
            Pattern::Prefix(p) => subject.starts_with(p.as_str()),
            Pattern::Glob {
                pattern,
                cross_slash,
            } => glob_match(pattern, subject, *cross_slash),
            Pattern::Domain(host) => domain_matches(host, subject),
        }
    }
}

fn domain_matches(rule_host: &str, url: &str) -> bool {
    // Extract the host from a URL or bare host, lowercase, strip a leading www.
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split_once(':').map_or(host_port, |(h, _)| h);
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let rule = rule_host.trim().to_ascii_lowercase();
    let rule = rule.strip_prefix("www.").unwrap_or(&rule);
    host == rule || host.ends_with(&format!(".{rule}"))
}

/// One permission rule.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    /// What to do when the rule matches.
    pub action: RuleAction,
    /// The tool class the rule targets.
    pub tool: ToolClass,
    /// The content pattern, or `None` to match every call of the class.
    pub pattern: Option<Pattern>,
}

/// Failure to parse a rule string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuleParseError {
    /// The tool-name token was not recognised.
    #[error("unknown tool in rule: {0}")]
    UnknownTool(String),
    /// The rule string was malformed (e.g. unbalanced parentheses).
    #[error("malformed rule: {0}")]
    Malformed(String),
}

impl PermissionRule {
    /// Build a rule directly (used by the config layer's structured form).
    pub fn new(action: RuleAction, tool: ToolClass, pattern: Option<Pattern>) -> Self {
        Self {
            action,
            tool,
            pattern,
        }
    }

    /// Parse the string form of a rule, pairing it with `action`.
    ///
    /// Accepts `Class`, `Class(inner)`, and the bare wildcard `*`. The `inner`
    /// is interpreted per class: a `Bash(git commit:*)` `:*` suffix becomes a
    /// plain prefix; `WebFetch(domain:host)` becomes a [`Domain`](Pattern::Domain)
    /// matcher; path classes glob without slash-crossing; the rest glob across.
    pub fn parse(action: RuleAction, s: &str) -> Result<Self, RuleParseError> {
        let s = s.trim();
        if s == "*" {
            return Ok(Self::new(action, ToolClass::Any, None));
        }

        let (token, inner) = match s.find('(') {
            Some(open) => {
                if !s.ends_with(')') {
                    return Err(RuleParseError::Malformed(s.to_string()));
                }
                (&s[..open], Some(&s[open + 1..s.len() - 1]))
            }
            None => (s, None),
        };

        let tool =
            ToolClass::parse(token).ok_or_else(|| RuleParseError::UnknownTool(token.to_string()))?;

        let pattern = match inner {
            None => None,
            Some("") => None,
            Some(inner) => Some(build_pattern(tool, inner)),
        };

        Ok(Self::new(action, tool, pattern))
    }
}

fn build_pattern(tool: ToolClass, inner: &str) -> Pattern {
    if tool == ToolClass::WebFetch {
        if let Some(host) = inner.strip_prefix("domain:") {
            return Pattern::Domain(host.to_string());
        }
    }

    // Normalise the MCP `server__tool` form to the dotted names Dadhichi uses.
    let inner_owned;
    let inner = if tool == ToolClass::Mcp && inner.contains("__") {
        inner_owned = inner.replace("__", ".");
        inner_owned.as_str()
    } else {
        inner
    };

    // Bash: a trailing `:*` is a prefix; a bare word with no glob metachar is a
    // prefix; otherwise a slash-crossing glob.
    if tool == ToolClass::Bash {
        if let Some(prefix) = inner.strip_suffix(":*") {
            return Pattern::Prefix(prefix.to_string());
        }
        if !inner.contains(['*', '?', '[']) {
            return Pattern::Prefix(inner.to_string());
        }
    }

    Pattern::Glob {
        pattern: inner.to_string(),
        cross_slash: tool.cross_slash(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tools_by_name() {
        assert_eq!(ToolClass::of("terminal.run"), ToolClass::Bash);
        assert_eq!(ToolClass::of("fs.read"), ToolClass::Read);
        assert_eq!(ToolClass::of("fs.write"), ToolClass::Edit);
        assert_eq!(ToolClass::of("fs.grep"), ToolClass::Grep);
        assert_eq!(ToolClass::of("github.create_issue"), ToolClass::Mcp);
        assert_eq!(ToolClass::of("echo"), ToolClass::Other);
    }

    #[test]
    fn parses_bare_class() {
        let r = PermissionRule::parse(RuleAction::Allow, "Read").unwrap();
        assert_eq!(r.tool, ToolClass::Read);
        assert!(r.pattern.is_none());
    }

    #[test]
    fn parses_wildcard() {
        let r = PermissionRule::parse(RuleAction::Deny, "*").unwrap();
        assert_eq!(r.tool, ToolClass::Any);
        assert!(r.pattern.is_none());
    }

    #[test]
    fn bash_prefix_vs_glob() {
        // Bare word → prefix.
        let r = PermissionRule::parse(RuleAction::Allow, "Bash(git)").unwrap();
        assert!(matches!(r.pattern, Some(Pattern::Prefix(p)) if p == "git"));
        // `:*` suffix → prefix.
        let r = PermissionRule::parse(RuleAction::Allow, "Bash(git commit:*)").unwrap();
        assert!(matches!(r.pattern, Some(Pattern::Prefix(p)) if p == "git commit"));
        // Contains `*` → glob.
        let r = PermissionRule::parse(RuleAction::Allow, "Bash(git *)").unwrap();
        assert!(matches!(r.pattern, Some(Pattern::Glob { .. })));
    }

    #[test]
    fn path_glob_does_not_cross_slash() {
        let r = PermissionRule::parse(RuleAction::Deny, "Read(src/*)").unwrap();
        match r.pattern.unwrap() {
            Pattern::Glob {
                pattern,
                cross_slash,
            } => {
                assert_eq!(pattern, "src/*");
                assert!(!cross_slash);
            }
            _ => panic!("expected glob"),
        }
    }

    #[test]
    fn mcp_double_underscore_normalised() {
        let r = PermissionRule::parse(RuleAction::Allow, "MCPTool(linear__*)").unwrap();
        match r.pattern.unwrap() {
            Pattern::Glob { pattern, .. } => assert_eq!(pattern, "linear.*"),
            _ => panic!("expected glob"),
        }
    }

    #[test]
    fn webfetch_domain() {
        let r = PermissionRule::parse(RuleAction::Allow, "WebFetch(domain:example.com)").unwrap();
        let p = r.pattern.unwrap();
        assert!(p.matches("https://api.example.com/x"));
        assert!(p.matches("http://example.com"));
        assert!(!p.matches("https://notexample.com"));
    }

    #[test]
    fn unknown_tool_errors() {
        let err = PermissionRule::parse(RuleAction::Allow, "Frobnicate(x)").unwrap_err();
        assert_eq!(err, RuleParseError::UnknownTool("Frobnicate".into()));
    }

    #[test]
    fn malformed_errors() {
        assert!(matches!(
            PermissionRule::parse(RuleAction::Allow, "Bash(git"),
            Err(RuleParseError::Malformed(_))
        ));
    }
}
