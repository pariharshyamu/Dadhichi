//! The Problems panel: a live view of diagnostics.
//!
//! It is a pure projection of `lsp.diagnostics` events published by
//! [`dadhichi-lsp`](https://docs.rs). Each event carries the full diagnostic set
//! for one file, so [`apply`](ProblemsPanel::apply) replaces that file's entries
//! wholesale — the panel never accumulates stale problems.

use std::collections::BTreeMap;

/// A single diagnostic row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The file URI or path the problem is in.
    pub file: String,
    /// 0-based line, as reported by the language server.
    pub line: u32,
    /// Severity label (`error`, `warning`, `information`, `hint`).
    pub severity: String,
    /// The diagnostic message.
    pub message: String,
}

/// Aggregates diagnostics per file for display.
#[derive(Debug, Default)]
pub struct ProblemsPanel {
    by_file: BTreeMap<String, Vec<Problem>>,
    selected: usize,
}

impl ProblemsPanel {
    /// Create an empty panel.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an `lsp.diagnostics` event payload, replacing the file's entries.
    ///
    /// The payload shape mirrors LSP's `publishDiagnostics`:
    /// `{ "uri": ..., "diagnostics": [ { "range": { "start": { "line": .. } },
    /// "severity": "warning", "message": .. } ] }`.
    pub fn apply(&mut self, payload: &serde_json::Value) {
        let Some(uri) = payload.get("uri").and_then(|u| u.as_str()) else {
            return;
        };
        let diagnostics = payload
            .get("diagnostics")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();

        let items: Vec<Problem> = diagnostics
            .iter()
            .filter_map(|d| {
                Some(Problem {
                    file: uri.to_string(),
                    line: d
                        .get("range")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.get("line"))
                        .and_then(|l| l.as_u64())
                        .unwrap_or(0) as u32,
                    severity: d
                        .get("severity")
                        .and_then(|s| s.as_str())
                        .unwrap_or("error")
                        .to_string(),
                    message: d.get("message")?.as_str()?.to_string(),
                })
            })
            .collect();

        if items.is_empty() {
            self.by_file.remove(uri);
        } else {
            self.by_file.insert(uri.to_string(), items);
        }
        self.clamp_selection();
    }

    /// Every problem across all files, ordered by file then line.
    pub fn all(&self) -> Vec<Problem> {
        self.by_file.values().flatten().cloned().collect()
    }

    /// The most severe diagnostic on the 0-based `line` of the file at `path`,
    /// if any, as a severity label. Diagnostics are keyed by LSP URI while the
    /// editor knows a filesystem path, so the two are matched on a trailing
    /// path-component boundary (`file:///a/b.rs` ↔ `/a/b.rs`). Used to mark the
    /// editor gutter beside lines that have problems.
    pub fn severity_on_line(&self, path: &str, line: u32) -> Option<&str> {
        let mut best: Option<&str> = None;
        for problems in self.by_file.values() {
            for p in problems {
                if p.line == line && paths_match(&p.file, path) {
                    let rank = |s: &str| match s {
                        "error" => 3,
                        "warning" => 2,
                        "information" => 1,
                        _ => 0,
                    };
                    if best.is_none_or(|b| rank(&p.severity) > rank(b)) {
                        best = Some(&p.severity);
                    }
                }
            }
        }
        best
    }

    /// Total problem count.
    pub fn count(&self) -> usize {
        self.by_file.values().map(Vec::len).sum()
    }

    /// The highlighted row index.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Move the selection down (saturating at the last row).
    pub fn select_next(&mut self) {
        let n = self.count();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    /// Move the selection up.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn clamp_selection(&mut self) {
        let n = self.count();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
    }
}

/// Whether an LSP diagnostic's file (a `file://` URI or bare path) refers to the
/// same file as the editor's `doc` path. Matches when they are equal after
/// stripping the URI scheme, or when one is a suffix of the other on a path
/// separator boundary (so a relative editor path lines up with an absolute URI).
fn paths_match(diag_file: &str, doc: &str) -> bool {
    let d = diag_file.strip_prefix("file://").unwrap_or(diag_file);
    d == doc || ends_on_boundary(d, doc) || ends_on_boundary(doc, d)
}

/// `long` ends with `short` at a `/` boundary (or equals it).
fn ends_on_boundary(long: &str, short: &str) -> bool {
    match long.len().cmp(&short.len()) {
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => long == short,
        std::cmp::Ordering::Greater => {
            long.ends_with(short) && long[..long.len() - short.len()].ends_with('/')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(uri: &str, line: u64, sev: &str, msg: &str) -> serde_json::Value {
        serde_json::json!({
            "uri": uri,
            "diagnostics": [{
                "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 1 } },
                "severity": sev,
                "message": msg
            }]
        })
    }

    #[test]
    fn applies_and_counts_diagnostics() {
        let mut panel = ProblemsPanel::new();
        panel.apply(&diag("file:///a.rs", 3, "warning", "unused import"));
        panel.apply(&diag("file:///b.rs", 0, "error", "type mismatch"));

        assert_eq!(panel.count(), 2);
        let all = panel.all();
        assert_eq!(all[0].file, "file:///a.rs");
        assert_eq!(all[0].line, 3);
        assert_eq!(all[0].severity, "warning");
    }

    #[test]
    fn reapplying_a_file_replaces_its_problems() {
        let mut panel = ProblemsPanel::new();
        panel.apply(&diag("file:///a.rs", 3, "warning", "first"));
        panel.apply(&diag("file:///a.rs", 9, "error", "second"));
        assert_eq!(panel.count(), 1);
        assert_eq!(panel.all()[0].message, "second");
    }

    #[test]
    fn severity_on_line_matches_uris_to_paths() {
        let mut panel = ProblemsPanel::new();
        panel.apply(&diag("file:///home/x/src/main.rs", 4, "warning", "unused"));
        panel.apply(&diag(
            "file:///home/x/src/main.rs",
            4,
            "error",
            "type error",
        ));

        // The URI matches the editor's filesystem path, and error outranks warning.
        assert_eq!(
            panel.severity_on_line("/home/x/src/main.rs", 4),
            Some("error")
        );
        // A relative path suffix also lines up on a separator boundary.
        assert_eq!(
            panel.severity_on_line("src/main.rs", 4),
            Some("error"),
            "relative suffix matches"
        );
        // No diagnostic on other lines, and no false match on a partial name.
        assert_eq!(panel.severity_on_line("/home/x/src/main.rs", 5), None);
        assert_eq!(panel.severity_on_line("ain.rs", 4), None, "not a boundary");
    }

    #[test]
    fn empty_diagnostics_clears_the_file() {
        let mut panel = ProblemsPanel::new();
        panel.apply(&diag("file:///a.rs", 3, "warning", "x"));
        panel.apply(&serde_json::json!({ "uri": "file:///a.rs", "diagnostics": [] }));
        assert_eq!(panel.count(), 0);
    }
}
