//! # dadhichi-config
//!
//! Layered configuration for Dadhichi's permission engine. Rules and the
//! session mode are read from several TOML sources and merged into one
//! [`Config`], following the model grok-build popularised:
//!
//! | Precedence (low → high) | Source | Purpose |
//! |---|---|---|
//! | 1 | `~/.dadhichi/config.toml` | personal defaults, all projects |
//! | 2 | `<dir>/.dadhichi/config.toml` (repo root → cwd) | per-project, committed |
//! | 3 | `/etc/dadhichi/requirements.toml` | root-owned enterprise lock |
//!
//! Permission **rules** from every source are merged into a single
//! [`RuleSet`](dadhichi_mcp::RuleSet); because the engine resolves by severity
//! (`deny > ask > allow`), a global `deny` can never be undone by a project
//! `allow`. The **mode** is taken from the most specific source that sets it,
//! and the enterprise `requirements.toml` can lock always-approve off.
//!
//! The loader is pure and side-effect free apart from reading files, and the
//! [`Config::load_layered`] entry point takes explicit paths so it can be tested
//! without touching a real `HOME`.

use dadhichi_mcp::{RuleAction, RuleSet, SessionMode};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The merged configuration that seeds the permission engine.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Content-aware permission rules from every source.
    pub rules: RuleSet,
    /// The session-wide fall-through mode.
    pub mode: SessionMode,
    /// Whether always-approve (`bypassPermissions`) is locked off by an
    /// enterprise `requirements.toml`. When set, a `mode` of `bypass` is
    /// downgraded to `default`.
    pub bypass_locked: bool,
    /// The requested OS sandbox profile name (`workspace` / `read-only` /
    /// `strict` / `off`), from `[sandbox] profile`. The binary resolves and
    /// applies it; `None` means no sandbox.
    pub sandbox: Option<String>,
    /// Non-fatal problems encountered while loading (bad rule strings, unknown
    /// tools, unreadable optional files). Loading never fails on these.
    pub warnings: Vec<String>,
}

impl Config {
    /// Load configuration for a working directory using the default source
    /// locations: the global `~/.dadhichi/config.toml` (honouring a
    /// `DADHICHI_HOME` override), every `<dir>/.dadhichi/config.toml` from the
    /// filesystem root down to `cwd`, and `/etc/dadhichi/requirements.toml`.
    pub fn load(cwd: &Path) -> Config {
        let mut sources = Vec::new();
        if let Some(home) = home_dir() {
            sources.push(home.join(".dadhichi").join("config.toml"));
        }
        sources.extend(project_config_chain(cwd));
        let requirements = PathBuf::from("/etc/dadhichi/requirements.toml");
        Config::load_layered(&sources, Some(&requirements))
    }

    /// Load from an explicit, ordered list of config sources (lowest precedence
    /// first) plus an optional enterprise `requirements` file. Missing files are
    /// skipped silently; malformed ones contribute a warning but don't abort.
    pub fn load_layered(sources: &[PathBuf], requirements: Option<&Path>) -> Config {
        let mut config = Config::default();

        for path in sources {
            config.merge_file(path);
        }

        if let Some(req) = requirements {
            if let Some(raw) = read_raw(req, &mut config.warnings) {
                // An enterprise file contributes rules and can set a mode like
                // any source, but additionally carries the bypass lock.
                config.apply_permission(raw.permission);
                config.apply_sandbox(raw.sandbox);
                if raw
                    .ui
                    .and_then(|u| u.disable_bypass_permissions_mode)
                    .unwrap_or(false)
                {
                    config.bypass_locked = true;
                }
            }
        }

        if config.bypass_locked && config.mode == SessionMode::Bypass {
            config.mode = SessionMode::Default;
        }
        config
    }

    fn merge_file(&mut self, path: &Path) {
        if let Some(raw) = read_raw(path, &mut self.warnings) {
            self.apply_permission(raw.permission);
            self.apply_sandbox(raw.sandbox);
        }
    }

    fn apply_sandbox(&mut self, sandbox: Option<RawSandbox>) {
        if let Some(profile) = sandbox.and_then(|s| s.profile) {
            self.sandbox = Some(profile);
        }
    }

    fn apply_permission(&mut self, permission: Option<RawPermission>) {
        let Some(perm) = permission else { return };

        // Compact string-array forms.
        for (action, list) in [
            (RuleAction::Deny, perm.deny),
            (RuleAction::Ask, perm.ask),
            (RuleAction::Allow, perm.allow),
        ] {
            if let Some(list) = list {
                for err in self.rules.add_strings(action, list) {
                    self.warnings.push(err.to_string());
                }
            }
        }

        // Structured `rules = [{ action, tool, pattern }]` form.
        for raw in perm.rules.unwrap_or_default() {
            match raw.into_string_rule() {
                Ok((action, s)) => {
                    for err in self.rules.add_strings(action, [s]) {
                        self.warnings.push(err.to_string());
                    }
                }
                Err(w) => self.warnings.push(w),
            }
        }

        // A mode set by a later (more specific) source wins.
        if let Some(name) = perm.default_mode {
            match SessionMode::parse(&name) {
                Some(mode) => self.mode = mode,
                None => self.warnings.push(format!("unknown defaultMode: {name}")),
            }
        }
    }
}

/// Read and parse one TOML file. `None` when the file is absent (not an error);
/// a parse error is pushed to `warnings` and yields `None`.
fn read_raw(path: &Path, warnings: &mut Vec<String>) -> Option<RawConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<RawConfig>(&text) {
        Ok(raw) => Some(raw),
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

/// The `.dadhichi/config.toml` chain from the filesystem root down to `cwd`,
/// so deeper (more specific) directories take precedence.
fn project_config_chain(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse(); // root first, cwd last
    dirs.into_iter()
        .map(|d| d.join(".dadhichi").join("config.toml"))
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("DADHICHI_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

// ── Raw TOML shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawConfig {
    permission: Option<RawPermission>,
    ui: Option<RawUi>,
    sandbox: Option<RawSandbox>,
}

#[derive(Debug, Deserialize)]
struct RawSandbox {
    profile: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPermission {
    #[serde(rename = "defaultMode")]
    default_mode: Option<String>,
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
    ask: Option<Vec<String>>,
    rules: Option<Vec<RawRule>>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    action: String,
    tool: String,
    pattern: Option<String>,
}

impl RawRule {
    /// Convert the structured form into an `(action, "Tool(pattern)")` pair the
    /// rule parser understands.
    fn into_string_rule(self) -> Result<(RuleAction, String), String> {
        let action = match self.action.as_str() {
            "allow" => RuleAction::Allow,
            "ask" => RuleAction::Ask,
            "deny" => RuleAction::Deny,
            other => return Err(format!("unknown rule action: {other}")),
        };
        let token = match self.tool.as_str() {
            "bash" => "Bash",
            "read" => "Read",
            "edit" => "Edit",
            "grep" => "Grep",
            "mcp" => "MCPTool",
            "webfetch" => "WebFetch",
            "websearch" => "WebSearch",
            other => return Err(format!("unknown rule tool: {other}")),
        };
        let s = match self.pattern {
            Some(p) => format!("{token}({p})"),
            None => token.to_string(),
        };
        Ok((action, s))
    }
}

#[derive(Debug, Deserialize)]
struct RawUi {
    disable_bypass_permissions_mode: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_mcp::{RuleAction, ToolCall};
    use serde_json::json;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parses_compact_and_structured_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
                [permission]
                defaultMode = "dontAsk"
                allow = ["Bash(git *)"]
                deny  = ["Bash(rm -rf *)"]
                rules = [
                  { action = "ask", tool = "edit" },
                ]
            "#,
        );
        let cfg = Config::load_layered(&[path], None);
        assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
        assert_eq!(cfg.mode, SessionMode::DontAsk);

        // deny wins for rm.
        let rm = json!({ "command": "rm -rf /" });
        assert_eq!(
            cfg.rules.evaluate(&ToolCall::new("terminal.run", &rm)),
            Some(RuleAction::Deny)
        );
        // the structured ask rule applies to edits.
        let edit = json!({ "path": "a.rs" });
        assert_eq!(
            cfg.rules.evaluate(&ToolCall::new("fs.write", &edit)),
            Some(RuleAction::Ask)
        );
    }

    #[test]
    fn deeper_source_overrides_mode_but_deny_still_wins() {
        // Global allows git broadly and sets default; project denies a path and
        // switches to dontAsk. Rules from both merge; project mode wins.
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.toml",
            r#"
                [permission]
                defaultMode = "default"
                allow = ["Bash(git *)"]
            "#,
        );
        let project = write(
            dir.path(),
            "project.toml",
            r#"
                [permission]
                defaultMode = "dontAsk"
                deny = ["Read(secrets/**)"]
            "#,
        );
        let cfg = Config::load_layered(&[global, project], None);
        assert_eq!(cfg.mode, SessionMode::DontAsk);

        let secret = json!({ "path": "secrets/prod.env" });
        assert_eq!(
            cfg.rules.evaluate(&ToolCall::new("fs.read", &secret)),
            Some(RuleAction::Deny)
        );
    }

    #[test]
    fn requirements_locks_bypass_off() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            dir.path(),
            "config.toml",
            r#"
                [permission]
                defaultMode = "bypassPermissions"
            "#,
        );
        let req = write(
            dir.path(),
            "requirements.toml",
            r#"
                [ui]
                disable_bypass_permissions_mode = true
            "#,
        );
        let cfg = Config::load_layered(&[user], Some(&req));
        assert!(cfg.bypass_locked);
        // bypass was requested but is locked off ⇒ downgraded to default.
        assert_eq!(cfg.mode, SessionMode::Default);
    }

    #[test]
    fn reads_sandbox_profile_deeper_wins() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.toml",
            "[sandbox]\nprofile = \"workspace\"\n",
        );
        let project = write(dir.path(), "project.toml", "[sandbox]\nprofile = \"strict\"\n");
        let cfg = Config::load_layered(&[global, project], None);
        assert_eq!(cfg.sandbox.as_deref(), Some("strict"));
    }

    #[test]
    fn missing_files_are_not_errors() {
        let cfg = Config::load_layered(&[PathBuf::from("/no/such/config.toml")], None);
        assert!(cfg.warnings.is_empty());
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.mode, SessionMode::Default);
    }

    #[test]
    fn bad_rule_string_warns_but_keeps_going() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
                [permission]
                deny = ["Frobnicate(x)", "Bash(rm -rf *)"]
            "#,
        );
        let cfg = Config::load_layered(&[path], None);
        assert_eq!(cfg.warnings.len(), 1); // the bad one
        // the good rule still loaded.
        let rm = json!({ "command": "rm -rf /" });
        assert_eq!(
            cfg.rules.evaluate(&ToolCall::new("terminal.run", &rm)),
            Some(RuleAction::Deny)
        );
    }
}
