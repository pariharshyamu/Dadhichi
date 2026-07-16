//! Hook definitions and their discovery from disk.

use crate::event::HookEvent;
use dadhichi_security::TrustStore;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a hook came from — global hooks are always trusted; project hooks
/// require a folder-trust decision before they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `~/.dadhichi/hooks/*.json` — the user's own, always trusted.
    Global,
    /// `<project>/.dadhichi/hooks/*.json` — runs only in a trusted folder.
    Project,
}

/// What a hook does when it fires.
#[derive(Debug, Clone)]
pub enum Handler {
    /// Run a shell command / script, passing the event as JSON on stdin.
    Command {
        command: String,
        timeout: Duration,
        env: HashMap<String, String>,
    },
    /// POST the event envelope to a URL. (Parsed and recorded; HTTP delivery is
    /// not yet executed — such hooks currently fail open.)
    Http { url: String, timeout: Duration },
}

/// One resolved hook: an event, an optional tool-name matcher, and a handler.
#[derive(Debug, Clone)]
pub struct Hook {
    /// The event it fires on.
    pub event: HookEvent,
    /// A regex tested against the tool / notification name. `None` matches all.
    pub matcher: Option<Regex>,
    /// The action to take.
    pub handler: Handler,
    /// Its provenance (for the trust gate and the UI).
    pub source: Source,
    /// A display name (the file stem plus event).
    pub name: String,
}

impl Hook {
    /// Whether this hook applies to `tool_name` (only meaningful for events
    /// that [accept a matcher](HookEvent::accepts_matcher)).
    pub fn matches(&self, tool_name: Option<&str>) -> bool {
        match &self.matcher {
            None => true,
            Some(re) if self.event.accepts_matcher() => tool_name.is_some_and(|n| re.is_match(n)),
            Some(_) => true, // matcher ignored for non-tool events
        }
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 5;

// ── Raw JSON shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawFile {
    hooks: HashMap<String, Vec<RawGroup>>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    matcher: Option<String>,
    hooks: Vec<RawHandler>,
}

#[derive(Debug, Deserialize)]
struct RawHandler {
    #[serde(rename = "type")]
    kind: String,
    command: Option<String>,
    url: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// Parse the hooks defined in one JSON file. Unknown events and malformed
/// handlers are skipped with a warning rather than failing the whole file.
pub fn parse_file(
    text: &str,
    source: Source,
    stem: &str,
    warnings: &mut Vec<String>,
) -> Vec<Hook> {
    let raw: RawFile = match serde_json::from_str(text) {
        Ok(raw) => raw,
        Err(e) => {
            warnings.push(format!("{stem}: {e}"));
            return Vec::new();
        }
    };

    let mut hooks = Vec::new();
    for (event_name, groups) in raw.hooks {
        let Some(event) = HookEvent::parse(&event_name) else {
            continue; // unknown event — skip silently for cross-tool compat
        };
        for group in groups {
            let matcher = match &group.matcher {
                Some(m) if !m.is_empty() => match Regex::new(m) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        warnings.push(format!("{stem}: bad matcher `{m}`: {e}"));
                        continue;
                    }
                },
                _ => None,
            };
            for handler in group.hooks {
                match resolve_handler(handler, stem, warnings) {
                    Some(h) => hooks.push(Hook {
                        event,
                        matcher: matcher.clone(),
                        handler: h,
                        source,
                        name: format!("{stem}:{}", event.as_str()),
                    }),
                    None => continue,
                }
            }
        }
    }
    hooks
}

fn resolve_handler(raw: RawHandler, stem: &str, warnings: &mut Vec<String>) -> Option<Handler> {
    let timeout = Duration::from_secs(raw.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
    match raw.kind.as_str() {
        "command" => match raw.command {
            Some(command) => Some(Handler::Command {
                command,
                timeout,
                env: raw.env,
            }),
            None => {
                warnings.push(format!("{stem}: command hook missing `command`"));
                None
            }
        },
        "http" => match raw.url {
            Some(url) => Some(Handler::Http { url, timeout }),
            None => {
                warnings.push(format!("{stem}: http hook missing `url`"));
                None
            }
        },
        other => {
            warnings.push(format!("{stem}: unknown hook type `{other}`"));
            None
        }
    }
}

/// Discover all hooks that apply to `cwd`.
///
/// Global hooks (`<home>/.dadhichi/hooks/*.json`) always load. Project hooks
/// (`<cwd>/.dadhichi/hooks/*.json`) load only when `trust` marks `cwd` trusted;
/// otherwise they are skipped and a note is added to `warnings`.
pub fn discover(
    home: Option<&Path>,
    cwd: &Path,
    trust: &TrustStore,
) -> (Vec<Hook>, Vec<String>) {
    let mut hooks = Vec::new();
    let mut warnings = Vec::new();

    if let Some(home) = home {
        let dir = home.join(".dadhichi").join("hooks");
        load_dir(&dir, Source::Global, &mut hooks, &mut warnings);
    }

    let project_dir = cwd.join(".dadhichi").join("hooks");
    if project_dir.is_dir() {
        if trust.is_trusted(cwd) {
            load_dir(&project_dir, Source::Project, &mut hooks, &mut warnings);
        } else {
            warnings.push(format!(
                "skipped project hooks in {} (folder not trusted)",
                project_dir.display()
            ));
        }
    }

    (hooks, warnings)
}

fn load_dir(dir: &Path, source: Source, hooks: &mut Vec<Hook>, warnings: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // no hooks dir — fine
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("hook")
            .to_string();
        match std::fs::read_to_string(&path) {
            Ok(text) => hooks.extend(parse_file(&text, source, &stem, warnings)),
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_pre_tool_use_hook() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [
                        { "type": "command", "command": "guard.sh", "timeout": 3 }
                    ]}
                ]
            }
        }"#;
        let mut warnings = Vec::new();
        let hooks = parse_file(json, Source::Global, "guard", &mut warnings);
        assert!(warnings.is_empty());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreToolUse);
        assert!(hooks[0].matches(Some("Bash")));
        assert!(!hooks[0].matches(Some("Read")));
    }

    #[test]
    fn unknown_event_is_skipped_not_fatal() {
        let json = r#"{ "hooks": { "MadeUpEvent": [ { "hooks": [
            { "type": "command", "command": "x" } ] } ] } }"#;
        let mut warnings = Vec::new();
        let hooks = parse_file(json, Source::Global, "x", &mut warnings);
        assert!(hooks.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn discovery_gates_project_hooks_on_trust() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // A project hook file.
        let hooks_dir = project.path().join(".dadhichi").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("p.json"),
            r#"{ "hooks": { "PreToolUse": [ { "hooks": [
                { "type": "command", "command": "x" } ] } ] } }"#,
        )
        .unwrap();

        let trust = TrustStore::new(home.path().join("trust.json"));

        // Untrusted: project hooks skipped.
        let (hooks, warnings) = discover(Some(home.path()), project.path(), &trust);
        assert!(hooks.is_empty());
        assert!(warnings.iter().any(|w| w.contains("not trusted")));

        // Trusted: they load.
        trust.trust(project.path()).unwrap();
        let (hooks, _) = discover(Some(home.path()), project.path(), &trust);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].source, Source::Project);
    }
}
