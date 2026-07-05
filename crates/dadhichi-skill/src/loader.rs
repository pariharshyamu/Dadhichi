//! Loading skills from disk.
//!
//! Skills are plain JSON manifests (the [`Skill`](crate::Skill) type round-trips
//! through Serde), so a directory of `*.json` files *is* a skill library. This
//! module reads those directories into a [`SkillRegistry`], tolerating malformed
//! files by collecting them into a [`LoadReport`] rather than aborting the whole
//! load.
//!
//! Standard locations, in increasing precedence (later wins, because
//! registering a skill replaces any earlier one of the same name):
//!
//! 1. `~/.dadhichi/skills` — the user's personal library.
//! 2. `<project_root>/.dadhichi/skills` — project-local skills, checked into a repo.
//! 3. `$DADHICHI_SKILLS_DIR` — an explicit override, highest precedence.
//!
//! [`SkillRegistry::discover_in`] layers the built-in library under all three;
//! [`SkillRegistry::discover`] does the same using the process cwd as the root.

use crate::registry::SkillRegistry;
use crate::skill::Skill;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// A single manifest that could not be loaded.
#[derive(Debug, Clone)]
pub struct LoadError {
    /// The offending file.
    pub path: PathBuf,
    /// Why it was rejected (parse error, unreadable, invalid skill).
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for LoadError {}

/// The outcome of loading skills from disk: what loaded, and what failed.
#[derive(Debug, Default, Clone)]
pub struct LoadReport {
    /// Names of the skills successfully loaded, in load order.
    pub loaded: Vec<String>,
    /// Files that could not be loaded, with the reason.
    pub errors: Vec<LoadError>,
}

impl LoadReport {
    /// Whether nothing was loaded and nothing failed.
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.errors.is_empty()
    }

    /// Whether any file failed to load.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Fold another report into this one.
    fn extend(&mut self, other: LoadReport) {
        self.loaded.extend(other.loaded);
        self.errors.extend(other.errors);
    }
}

/// Resolve the standard skill directories from an arbitrary variable lookup and
/// project root.
///
/// Pure and injectable so precedence is unit-testable without mutating the real
/// process environment (setting env vars is `unsafe` under edition 2024).
fn resolve_dirs(project_root: &Path, get: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let nonempty = |v: OsString| (!v.is_empty()).then_some(v);
    let mut dirs = Vec::new();

    if let Some(home) = get("HOME")
        .or_else(|| get("USERPROFILE"))
        .and_then(nonempty)
    {
        dirs.push(PathBuf::from(home).join(".dadhichi").join("skills"));
    }
    dirs.push(project_root.join(".dadhichi").join("skills"));
    if let Some(explicit) = get("DADHICHI_SKILLS_DIR").and_then(nonempty) {
        dirs.push(PathBuf::from(explicit));
    }
    dirs
}

/// The standard skill directories for the current working directory, in
/// increasing precedence. Missing directories are simply skipped when loaded.
pub fn skill_dirs() -> Vec<PathBuf> {
    skill_dirs_in(Path::new("."))
}

/// The standard skill directories, with project-local skills resolved relative
/// to `project_root` (the opened workspace) rather than the process cwd.
pub fn skill_dirs_in(project_root: impl AsRef<Path>) -> Vec<PathBuf> {
    resolve_dirs(project_root.as_ref(), |key| std::env::var_os(key))
}

impl SkillRegistry {
    /// Load every `*.json` skill manifest in `dir`, registering each.
    ///
    /// A missing or unreadable directory yields an empty report (not an error);
    /// individual malformed manifests are collected into the report. Files are
    /// processed in sorted order for determinism.
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) -> LoadReport {
        let dir = dir.as_ref();
        let mut report = LoadReport::default();

        let Ok(entries) = std::fs::read_dir(dir) else {
            return report; // missing directory is not an error
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        paths.sort();

        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => match Skill::from_json(&text) {
                    Ok(skill) if skill.name.trim().is_empty() => {
                        report.errors.push(LoadError {
                            path,
                            message: "skill manifest has an empty `name`".to_string(),
                        });
                    }
                    Ok(skill) => {
                        let name = skill.name.clone();
                        self.register(skill);
                        report.loaded.push(name);
                    }
                    Err(err) => report.errors.push(LoadError {
                        path,
                        message: err.to_string(),
                    }),
                },
                Err(err) => report.errors.push(LoadError {
                    path,
                    message: err.to_string(),
                }),
            }
        }

        report
    }

    /// Load every standard directory into this registry, in precedence order
    /// (later directories override earlier same-named skills). Project-local
    /// skills are resolved relative to `project_root`.
    pub fn load_standard_dirs(&mut self, project_root: impl AsRef<Path>) -> LoadReport {
        let mut report = LoadReport::default();
        for dir in skill_dirs_in(project_root.as_ref()) {
            report.extend(self.load_dir(dir));
        }
        report
    }

    /// A registry of the built-in library plus every skill discovered on disk,
    /// with project-local skills taken from `<project_root>/.dadhichi/skills`.
    ///
    /// Disk skills override built-ins of the same name, so a user can customise
    /// a shipped skill by dropping a manifest with the same `name` into their
    /// `~/.dadhichi/skills`.
    pub fn discover_in(project_root: impl AsRef<Path>) -> (Self, LoadReport) {
        let mut registry = Self::with_builtins();
        let report = registry.load_standard_dirs(project_root);
        (registry, report)
    }

    /// [`SkillRegistry::discover_in`] using the process working directory for
    /// project-local skills.
    pub fn discover() -> (Self, LoadReport) {
        Self::discover_in(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn loads_valid_manifests() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "greeter.json",
            r#"{"name":"greeter","description":"say hi","tools":{"mode":"none"}}"#,
        );
        write(
            dir.path(),
            "reviewer.json",
            r#"{"name":"reviewer","description":"review",
                "required_permissions":["read_workspace"],
                "tools":{"mode":"allow","names":["fs.read"]}}"#,
        );

        let mut reg = SkillRegistry::new();
        let report = reg.load_dir(dir.path());

        assert_eq!(report.loaded, vec!["greeter", "reviewer"]);
        assert!(!report.has_errors());
        assert!(reg.contains("greeter"));
        assert!(reg.get("reviewer").unwrap().tools.allows("fs.read"));
    }

    #[test]
    fn malformed_manifest_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "good.json",
            r#"{"name":"good","description":"ok"}"#,
        );
        write(dir.path(), "bad.json", "{ this is not json");
        write(
            dir.path(),
            "empty-name.json",
            r#"{"name":"  ","description":"x"}"#,
        );
        write(dir.path(), "ignored.txt", "not a manifest");

        let mut reg = SkillRegistry::new();
        let report = reg.load_dir(dir.path());

        assert_eq!(report.loaded, vec!["good"]);
        assert_eq!(report.errors.len(), 2); // bad.json + empty-name.json
        assert!(reg.contains("good"));
    }

    #[test]
    fn missing_directory_is_empty_not_error() {
        let mut reg = SkillRegistry::new();
        let report = reg.load_dir("/no/such/dir/hopefully");
        assert!(report.is_empty());
    }

    #[test]
    fn disk_skill_overrides_a_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "explain.json",
            r#"{"name":"explain","description":"my custom explain"}"#,
        );

        let mut reg = SkillRegistry::with_builtins();
        assert_eq!(
            reg.get("explain").unwrap().description,
            "Explain code or a concept in plain language"
        );

        let report = reg.load_dir(dir.path());
        assert_eq!(report.loaded, vec!["explain"]);
        assert_eq!(reg.get("explain").unwrap().description, "my custom explain");
    }

    #[test]
    fn precedence_is_home_then_project_then_env_override() {
        let env: std::collections::HashMap<&str, OsString> = [
            ("HOME", OsString::from("/home/u")),
            ("DADHICHI_SKILLS_DIR", OsString::from("/explicit/skills")),
        ]
        .into_iter()
        .collect();

        let dirs = resolve_dirs(Path::new("/proj"), |k| env.get(k).cloned());
        assert_eq!(dirs[0], PathBuf::from("/home/u/.dadhichi/skills"));
        assert_eq!(dirs[1], PathBuf::from("/proj/.dadhichi/skills"));
        assert_eq!(dirs[2], PathBuf::from("/explicit/skills"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let dirs = resolve_dirs(Path::new("."), |k| match k {
            "HOME" => Some(OsString::new()),
            "DADHICHI_SKILLS_DIR" => Some(OsString::new()),
            _ => None,
        });
        // Only the project-local directory remains.
        assert_eq!(dirs, vec![PathBuf::from("./.dadhichi/skills")]);
    }
}
