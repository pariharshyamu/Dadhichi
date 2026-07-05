//! Watching the skill manifest directories and hot-reloading on change.
//!
//! [`watch_skills`] starts a filesystem watch over the standard skill
//! directories ([`skill_dirs_in`](crate::loader::skill_dirs_in)). When a `*.json`
//! manifest is created, modified, or removed, it re-runs
//! [`SkillRegistry::discover_in`], swaps the shared catalogue in place, and
//! invokes a caller-supplied callback with the [`LoadReport`] — so a frontend
//! can publish events, refresh a picker, etc. Dropping the returned
//! [`SkillWatchGuard`] stops watching.
//!
//! The catalogue is shared behind a [`std::sync::RwLock`] (not an async lock)
//! because the watcher callback runs on the watcher's own thread; the critical
//! sections are tiny (swap the registry, read its length) and never held across
//! an `.await`.

use crate::loader::{LoadReport, skill_dirs_in};
use crate::registry::SkillRegistry;
use std::path::Path;
use std::sync::{Arc, RwLock};
use thiserror::Error;

/// A skill catalogue shared between the commands and the file watcher.
pub type SharedSkills = Arc<RwLock<SkillRegistry>>;

/// Wrap a registry as a [`SharedSkills`] handle.
pub fn shared(registry: SkillRegistry) -> SharedSkills {
    Arc::new(RwLock::new(registry))
}

/// Why starting the skill watcher failed.
#[derive(Debug, Error)]
pub enum SkillWatchError {
    /// The underlying filesystem watcher could not be created or armed.
    #[error("failed to start skill watcher: {0}")]
    Notify(String),
}

/// Keeps the skill-directory watches alive; dropping it stops watching.
pub struct SkillWatchGuard {
    _watchers: Vec<notify::RecommendedWatcher>,
}

impl std::fmt::Debug for SkillWatchGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillWatchGuard")
            .field("watched_dirs", &self._watchers.len())
            .finish()
    }
}

/// Watch the standard skill directories for `project_root` and hot-reload
/// `skills` when a manifest changes, calling `on_reload` with each reload's
/// report.
///
/// Only directories that currently exist are watched; if none do, the returned
/// guard simply watches nothing (manual reloads still work). `on_reload` runs on
/// the watcher thread, so keep it quick and non-blocking (e.g. publish a bus
/// event).
pub fn watch_skills<F>(
    skills: SharedSkills,
    project_root: impl AsRef<Path>,
    on_reload: F,
) -> Result<SkillWatchGuard, SkillWatchError>
where
    F: Fn(&LoadReport) + Send + Sync + 'static,
{
    use notify::{EventKind, RecursiveMode, Watcher};

    let root = project_root.as_ref().to_path_buf();

    // The reload action: re-discover, swap the catalogue, notify the caller.
    // Shared (Arc) so every per-directory watcher runs the same closure.
    let reload = {
        let skills = skills.clone();
        let root = root.clone();
        Arc::new(move || {
            let (fresh, report) = SkillRegistry::discover_in(&root);
            if let Ok(mut guard) = skills.write() {
                *guard = fresh;
            }
            // Lock released above before the callback observes the new state.
            on_reload(&report);
        })
    };

    let mut watchers = Vec::new();
    for dir in skill_dirs_in(&root) {
        if !dir.is_dir() {
            continue;
        }
        let reload = reload.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            // Only react when a JSON manifest is involved.
            let touches_manifest = event
                .paths
                .iter()
                .any(|path| path.extension().and_then(|e| e.to_str()) == Some("json"));
            if touches_manifest {
                (*reload)();
            }
        })
        .map_err(|e| SkillWatchError::Notify(e.to_string()))?;

        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|e| SkillWatchError::Notify(e.to_string()))?;
        watchers.push(watcher);
    }

    Ok(SkillWatchGuard {
        _watchers: watchers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn watcher_reloads_when_a_manifest_appears() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".dadhichi").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skills = shared(SkillRegistry::with_builtins());
        assert!(!skills.read().unwrap().contains("watched"));

        // The callback signals each reload over a channel.
        let (tx, rx) = mpsc::channel::<usize>();
        let _guard = watch_skills(skills.clone(), root.path(), move |report| {
            let _ = tx.send(report.loaded.len());
        })
        .expect("start watcher");

        // Author a new manifest; the watcher should reload and pick it up.
        std::fs::write(
            skill_dir.join("watched.json"),
            r#"{"name":"watched","description":"added while watching"}"#,
        )
        .unwrap();

        // Wait (bounded) for at least one reload to fire.
        rx.recv_timeout(Duration::from_secs(10))
            .expect("watcher fired a reload");

        // Give the swap a moment in case the signalling reload observed an
        // intermediate state, then assert the skill is now live.
        for _ in 0..50 {
            if skills.read().unwrap().contains("watched") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(skills.read().unwrap().contains("watched"));
    }

    #[test]
    fn watching_a_root_without_skill_dirs_is_ok() {
        let root = tempfile::tempdir().unwrap();
        let skills = shared(SkillRegistry::new());
        // No ~/.dadhichi or <root>/.dadhichi present: watcher starts, watches
        // nothing, and does not error.
        let guard = watch_skills(skills, root.path(), |_| {});
        assert!(guard.is_ok());
    }
}
