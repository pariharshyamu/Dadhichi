//! # dadhichi-session
//!
//! Persistent, resumable sessions. Where the CLI's cross-run memory blob keeps
//! only a recent window, a [`Session`] records the full conversation as an
//! append-only JSONL log on disk, so it can be **resumed**, **forked** into a
//! peer, or **rewound** to an earlier point (restoring the files as they were).
//! This mirrors the grok-build session model.
//!
//! ## Layout
//!
//! ```text
//! <root>/<group>/<id>/
//!   summary.json          index entry: id, title, cwd, timestamps, counts, parent
//!   updates.jsonl         append-only conversation/tool-call log (source of truth)
//!   rewind_points.jsonl   file snapshots, one record per snapshot
//! ```
//!
//! Sessions are grouped by working directory (a sanitised, hashed name) so
//! `list` can show the sessions for the current project.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The on-disk index entry for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Unique session id.
    pub id: String,
    /// A human title (model- or user-set), if any.
    pub title: Option<String>,
    /// The working directory the session belongs to.
    pub cwd: PathBuf,
    /// Creation time (unix seconds).
    pub created_at: u64,
    /// Last-update time (unix seconds).
    pub updated_at: u64,
    /// Number of events in `updates.jsonl`.
    pub num_events: usize,
    /// The session this was forked from, if any.
    pub parent: Option<String>,
}

/// A lightweight view of a rewind point, for listing (without the file bodies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPoint {
    /// Its index (the value passed to [`Session::rewind`]).
    pub index: usize,
    /// A short label (usually the prompt it precedes).
    pub label: String,
    /// When it was taken (unix seconds).
    pub ts: u64,
    /// The number of events at snapshot time.
    pub event_count: usize,
}

/// The store: a root directory holding all sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// A store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default store at `~/.dadhichi/sessions` (honouring `DADHICHI_HOME`).
    pub fn at_home() -> Option<Self> {
        let home = std::env::var_os("DADHICHI_HOME")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)?;
        Some(Self::new(home.join(".dadhichi").join("sessions")))
    }

    /// Start a fresh session for `cwd`.
    pub fn create(&self, cwd: &Path) -> io::Result<Session> {
        self.create_with_id(cwd, &new_id(), None)
    }

    /// Resume an existing session by id, from any working-directory group.
    pub fn resume(&self, id: &str) -> io::Result<Session> {
        let dir = self.find_dir(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("session {id} not found"))
        })?;
        let summary = read_summary(&dir)?;
        Ok(Session {
            dir,
            id: summary.id,
            cwd: summary.cwd,
        })
    }

    /// The most recently updated session for `cwd`, if any.
    pub fn latest(&self, cwd: &Path) -> Option<Session> {
        let summary = self.list(cwd).into_iter().next()?;
        self.resume(&summary.id).ok()
    }

    /// Fork `id` into a new peer session that starts from a copy of its history,
    /// recording the source as its `parent`.
    pub fn fork(&self, id: &str) -> io::Result<Session> {
        let src = self.resume(id)?;
        let child = self.create_with_id(&src.cwd, &new_id(), Some(id.to_string()))?;
        copy_if_exists(&src.updates_path(), &child.updates_path())?;
        copy_if_exists(
            &src.dir.join("rewind_points.jsonl"),
            &child.dir.join("rewind_points.jsonl"),
        )?;
        let src_events = read_summary(&src.dir)?.num_events;
        let mut s = read_summary(&child.dir)?;
        s.num_events = src_events;
        write_summary(&child.dir, &s)?;
        Ok(child)
    }

    /// The sessions for `cwd`, most-recently-updated first.
    pub fn list(&self, cwd: &Path) -> Vec<SessionSummary> {
        let group = self.root.join(group_name(cwd));
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(&group) {
            for e in entries.flatten() {
                if let Ok(s) = read_summary(&e.path()) {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    fn create_with_id(&self, cwd: &Path, id: &str, parent: Option<String>) -> io::Result<Session> {
        let dir = self.root.join(group_name(cwd)).join(id);
        fs::create_dir_all(&dir)?;
        let now = now_secs();
        let summary = SessionSummary {
            id: id.to_string(),
            title: None,
            cwd: cwd.to_path_buf(),
            created_at: now,
            updated_at: now,
            num_events: 0,
            parent,
        };
        write_summary(&dir, &summary)?;
        // Ensure the log exists so a resume of an empty session works.
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("updates.jsonl"))?;
        Ok(Session {
            dir,
            id: id.to_string(),
            cwd: cwd.to_path_buf(),
        })
    }

    fn find_dir(&self, id: &str) -> Option<PathBuf> {
        let groups = fs::read_dir(&self.root).ok()?;
        for g in groups.flatten() {
            let candidate = g.path().join(id);
            if candidate.join("summary.json").is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

/// A single open session.
#[derive(Debug, Clone)]
pub struct Session {
    dir: PathBuf,
    id: String,
    cwd: PathBuf,
}

impl Session {
    /// Its id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The working directory it belongs to.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Its on-disk directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The current index entry.
    pub fn summary(&self) -> io::Result<SessionSummary> {
        read_summary(&self.dir)
    }

    /// Append one event to the conversation log.
    pub fn append(&self, event: &serde_json::Value) -> io::Result<()> {
        let line = serde_json::to_string(event)?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.updates_path())?;
        writeln!(f, "{line}")?;
        let mut s = read_summary(&self.dir)?;
        s.num_events += 1;
        s.updated_at = now_secs();
        write_summary(&self.dir, &s)
    }

    /// Every event in the log, in order (the basis for replay on resume).
    pub fn events(&self) -> io::Result<Vec<serde_json::Value>> {
        let text = match fs::read_to_string(self.updates_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// The `(role, text)` turns recorded in the log, for replaying prior
    /// context into an agent on resume. Events without both a string `role` and
    /// `text` are skipped.
    pub fn transcript(&self) -> io::Result<Vec<(String, String)>> {
        Ok(self
            .events()?
            .into_iter()
            .filter_map(|e| {
                let role = e.get("role")?.as_str()?.to_string();
                let text = e.get("text")?.as_str()?.to_string();
                Some((role, text))
            })
            .collect())
    }

    /// Set the session title.
    pub fn set_title(&self, title: &str) -> io::Result<()> {
        let mut s = read_summary(&self.dir)?;
        s.title = Some(title.to_string());
        s.updated_at = now_secs();
        write_summary(&self.dir, &s)
    }

    /// Record a rewind point: a snapshot of `files` (path + contents) plus the
    /// current event count. Returns the point's index for a later [`rewind`].
    ///
    /// [`rewind`]: Session::rewind
    pub fn snapshot(&self, label: &str, files: &[(PathBuf, String)]) -> io::Result<usize> {
        let index = self.rewind_records()?.len();
        let rec = RewindRecord {
            index,
            label: label.to_string(),
            ts: now_secs(),
            event_count: read_summary(&self.dir)?.num_events,
            files: files
                .iter()
                .map(|(p, c)| Snapshot {
                    path: p.clone(),
                    content: c.clone(),
                })
                .collect(),
        };
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("rewind_points.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        Ok(index)
    }

    /// The recorded rewind points (labels/timestamps, no file bodies).
    pub fn rewind_points(&self) -> io::Result<Vec<RewindPoint>> {
        Ok(self
            .rewind_records()?
            .into_iter()
            .map(|r| RewindPoint {
                index: r.index,
                label: r.label,
                ts: r.ts,
                event_count: r.event_count,
            })
            .collect())
    }

    /// Rewind to point `index`: restore each snapshotted file's contents to
    /// disk and truncate the event log back to that point. Returns the paths
    /// restored.
    pub fn rewind(&self, index: usize) -> io::Result<Vec<PathBuf>> {
        let records = self.rewind_records()?;
        let rec = records.get(index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("rewind point {index} not found"),
            )
        })?;

        let mut restored = Vec::new();
        for snap in &rec.files {
            if let Some(parent) = snap.path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&snap.path, &snap.content)?;
            restored.push(snap.path.clone());
        }

        truncate_lines(&self.updates_path(), rec.event_count)?;
        let mut s = read_summary(&self.dir)?;
        s.num_events = rec.event_count;
        s.updated_at = now_secs();
        write_summary(&self.dir, &s)?;
        Ok(restored)
    }

    fn updates_path(&self) -> PathBuf {
        self.dir.join("updates.jsonl")
    }

    fn rewind_records(&self) -> io::Result<Vec<RewindRecord>> {
        let path = self.dir.join("rewind_points.jsonl");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RewindRecord {
    index: usize,
    label: String,
    ts: u64,
    event_count: usize,
    files: Vec<Snapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    path: PathBuf,
    content: String,
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_summary(dir: &Path) -> io::Result<SessionSummary> {
    let bytes = fs::read(dir.join("summary.json"))?;
    serde_json::from_slice(&bytes).map_err(io::Error::from)
}

fn write_summary(dir: &Path, summary: &SessionSummary) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(summary)?;
    fs::write(dir.join("summary.json"), json)
}

fn copy_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::copy(from, to) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Truncate a newline-delimited file to its first `keep` lines.
fn truncate_lines(path: &Path, keep: usize) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let kept: Vec<&str> = text.lines().take(keep).collect();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(path, out)
}

/// A filesystem-safe, collision-resistant group name for a working directory:
/// a sanitised prefix of the path plus a stable hash of the full path.
fn group_name(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let mut safe: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if safe.len() > 80 {
        safe = safe.chars().rev().take(80).collect::<String>();
        safe = safe.chars().rev().collect();
    }
    format!("{safe}-{:016x}", stable_hash(&s))
}

fn stable_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    // DefaultHasher uses fixed keys, so this is deterministic across runs.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        (dir, store)
    }

    #[test]
    fn create_append_and_resume_replays_events() {
        let (tmp, store) = store();
        let cwd = tmp.path().join("proj");

        let session = store.create(&cwd).unwrap();
        session.append(&serde_json::json!({ "role": "user", "text": "hi" })).unwrap();
        session
            .append(&serde_json::json!({ "role": "assistant", "text": "hello" }))
            .unwrap();
        let id = session.id().to_string();

        // A fresh resume sees the same events in order.
        let resumed = store.resume(&id).unwrap();
        let events = resumed.events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["text"], "hi");
        assert_eq!(resumed.summary().unwrap().num_events, 2);
    }

    #[test]
    fn transcript_extracts_role_text_turns() {
        let (tmp, store) = store();
        let s = store.create(&tmp.path().join("p")).unwrap();
        s.append(&serde_json::json!({ "role": "user", "text": "hi" })).unwrap();
        s.append(&serde_json::json!({ "role": "assistant", "text": "hello" })).unwrap();
        s.append(&serde_json::json!({ "role": "system", "note": "no text field" })).unwrap();
        let t = s.transcript().unwrap();
        assert_eq!(t, vec![
            ("user".to_string(), "hi".to_string()),
            ("assistant".to_string(), "hello".to_string()),
        ]);
    }

    #[test]
    fn resume_unknown_id_errors() {
        let (_tmp, store) = store();
        assert!(store.resume("nope").is_err());
    }

    #[test]
    fn latest_returns_most_recent_for_cwd() {
        let (tmp, store) = store();
        let cwd = tmp.path().join("proj");
        let _a = store.create(&cwd).unwrap();
        let b = store.create(&cwd).unwrap();
        b.append(&serde_json::json!({ "x": 1 })).unwrap(); // makes b newer
        assert_eq!(store.latest(&cwd).unwrap().id(), b.id());
        assert_eq!(store.list(&cwd).len(), 2);
    }

    #[test]
    fn fork_copies_history_and_records_parent() {
        let (tmp, store) = store();
        let cwd = tmp.path().join("proj");
        let parent = store.create(&cwd).unwrap();
        parent.append(&serde_json::json!({ "x": 1 })).unwrap();

        let child = store.fork(parent.id()).unwrap();
        assert_ne!(child.id(), parent.id());
        assert_eq!(child.events().unwrap().len(), 1); // history copied
        assert_eq!(child.summary().unwrap().parent.as_deref(), Some(parent.id()));

        // Diverging the child does not touch the parent.
        child.append(&serde_json::json!({ "x": 2 })).unwrap();
        assert_eq!(parent.events().unwrap().len(), 1);
        assert_eq!(child.events().unwrap().len(), 2);
    }

    #[test]
    fn rewind_restores_files_and_truncates_log() {
        let (tmp, store) = store();
        let cwd = tmp.path().join("proj");
        let session = store.create(&cwd).unwrap();

        // A file at v1, snapshotted, then event + edit to v2.
        let file = tmp.path().join("code.rs");
        fs::write(&file, "v1").unwrap();
        session.append(&serde_json::json!({ "turn": 1 })).unwrap();
        let point = session
            .snapshot("before turn 2", &[(file.clone(), "v1".to_string())])
            .unwrap();
        session.append(&serde_json::json!({ "turn": 2 })).unwrap();
        fs::write(&file, "v2").unwrap();
        assert_eq!(session.events().unwrap().len(), 2);

        // Rewind restores the file and truncates the log to the snapshot point.
        let restored = session.rewind(point).unwrap();
        assert_eq!(restored, vec![file.clone()]);
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
        assert_eq!(session.events().unwrap().len(), 1);
        assert_eq!(session.summary().unwrap().num_events, 1);
    }

    #[test]
    fn rewind_points_list_without_bodies() {
        let (tmp, store) = store();
        let session = store.create(&tmp.path().join("p")).unwrap();
        session.snapshot("first", &[]).unwrap();
        session.snapshot("second", &[]).unwrap();
        let points = session.rewind_points().unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].label, "first");
        assert_eq!(points[1].index, 1);
    }
}
