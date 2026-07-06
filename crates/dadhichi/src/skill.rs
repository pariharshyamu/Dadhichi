//! `dadhichi skill …` operations: manage the on-disk skill library the agent
//! console and TUI load from `~/.dadhichi/skills`.
//!
//! `import` is how a user "pushes" a skill JSON file into the toolkit: it
//! validates the manifest, then copies it into the user skills directory under a
//! filename derived from the skill's name. Because the running TUI watches that
//! directory, an import made while it is open shows up in the `>` skill palette
//! live — no restart. `list` shows what is currently available.

use crate::cli::SkillCommand;
use dadhichi_skill::{Skill, SkillRegistry};
use std::path::{Path, PathBuf};

/// Execute a parsed `skill` subcommand, printing results and exiting non-zero on
/// failure.
pub fn run(cmd: SkillCommand) {
    let outcome = match cmd {
        SkillCommand::Help => {
            println!("{}", help_text());
            return;
        }
        SkillCommand::List => {
            list();
            return;
        }
        SkillCommand::Import { path } => import(&path),
    };
    if let Err(e) = outcome {
        eprintln!("skill: {e}");
        std::process::exit(1);
    }
}

/// The user skills directory new manifests are installed into:
/// `~/.dadhichi/skills`, falling back to `./.dadhichi/skills` when no home is
/// set. Both are standard load locations, so an installed skill is discovered.
fn install_dir() -> PathBuf {
    let nonempty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    match std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(nonempty)
    {
        Some(home) => PathBuf::from(home).join(".dadhichi").join("skills"),
        None => PathBuf::from(".").join(".dadhichi").join("skills"),
    }
}

/// Reduce a skill name to a safe, lowercase filename stem (mirrors the app's
/// importer), so a crafted name can't escape the skills directory.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Validate the manifest at `path` and install it into the user skills dir.
fn import(path: &str) -> Result<(), String> {
    let dest = install_into(&install_dir(), path)?;
    println!("installed skill → {}", dest.display());
    println!("it is now available to the agent console and TUI (`>` in the palette).");
    Ok(())
}

/// Read and validate the manifest at `path`, then write it into `dir` under a
/// filename derived from the skill's name. Returns the written path. Split from
/// [`import`] so the disk logic is testable without touching the environment.
fn install_into(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let skill = Skill::from_json(&text).map_err(|e| format!("invalid skill manifest: {e}"))?;
    if skill.name.trim().is_empty() {
        return Err("skill manifest has an empty `name`".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dest = dir.join(format!("{}.json", sanitize_filename(&skill.name)));
    std::fs::write(&dest, skill.to_json())
        .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Print the available skills, flagging which were loaded from disk.
fn list() {
    let (skills, load) = SkillRegistry::discover();
    println!("{} skill(s) available:", skills.len());
    let from_disk: std::collections::BTreeSet<&String> = load.loaded.iter().collect();
    for spec in skills.list() {
        let origin = if from_disk.contains(&spec.name) {
            "disk"
        } else {
            "built-in"
        };
        println!("  {:<24} [{origin}]  {}", spec.name, spec.description);
    }
    for err in &load.errors {
        eprintln!("  skipped malformed manifest: {err}");
    }
}

fn help_text() -> String {
    "USAGE:
    dadhichi skill import PATH    Validate a skill JSON manifest and install it
                                  into ~/.dadhichi/skills.
    dadhichi skill list          List available skills (built-in + on disk).

A skill is a JSON manifest describing a permission-scoped capability an agent can
equip. Installed skills are picked up by the agent console and the TUI (the `>`
palette). A running TUI reloads them live via its file watcher."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_folds_unsafe_characters() {
        assert_eq!(sanitize_filename("Code Review!"), "code-review");
        assert_eq!(sanitize_filename("../evil"), "evil");
        assert_eq!(sanitize_filename("///"), "skill");
    }

    #[test]
    fn install_validates_and_writes_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");

        let manifest = dir.path().join("my.json");
        std::fs::write(
            &manifest,
            r#"{"name":"My Skill","description":"does things","instructions":"do it"}"#,
        )
        .unwrap();

        let dest = install_into(&skills, manifest.to_str().unwrap()).unwrap();

        // Written under the sanitised name, in the target directory.
        assert_eq!(dest, skills.join("my-skill.json"));
        assert!(dest.exists());
        let text = std::fs::read_to_string(&dest).unwrap();
        assert!(text.contains("My Skill"));
    }

    #[test]
    fn install_rejects_malformed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("bad.json");
        std::fs::write(&manifest, "not json").unwrap();
        assert!(install_into(&dir.path().join("skills"), manifest.to_str().unwrap()).is_err());
    }
}
