//! # dadhichi-ui
//!
//! The **toolkit-agnostic UI application core** — the shell's brain. It owns the
//! view-models (editor, explorer, command palette, problems, chat/agent
//! console) and applies kernel events to them, but renders nothing itself. A
//! concrete frontend ([`dadhichi-tui`](https://docs.rs) today; a `wgpu`/GPUI
//! shell later) reads these view-models and draws them, so the same application
//! logic backs every renderer.
//!
//! This realises the architecture's one-way data flow: **UI intent → command →
//! service → event → view-model update**. The UI holds no authoritative state —
//! [`App::apply_event`] is the single place bus events mutate it.
//!
//! ```
//! use dadhichi_ui::{App, Focus};
//! use dadhichi_core::Event;
//!
//! let mut app = App::new();
//! app.set_commands(vec!["agent.run".into(), "editor.save".into()]);
//!
//! // A diagnostics event flows straight into the Problems panel.
//! app.apply_event(&Event::new("lsp.diagnostics", serde_json::json!({
//!     "uri": "file:///a.rs",
//!     "diagnostics": [{ "range": { "start": { "line": 2 } }, "severity": "warning", "message": "unused" }]
//! })));
//! assert_eq!(app.problems.count(), 1);
//! ```

pub mod document;
pub mod explorer;
pub mod palette;
pub mod problems;

pub use document::Document;
pub use explorer::Explorer;
pub use palette::{CommandPalette, PaletteAction, PaletteItem, SkillEntry};
pub use problems::ProblemsPanel;

use dadhichi_core::Event;
use std::path::PathBuf;

/// Which panel currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The file explorer.
    Explorer,
    /// The editor.
    Editor,
    /// The problems list.
    Problems,
    /// The chat / agent console.
    Chat,
    /// The command palette (a modal overlay).
    Palette,
}

impl Focus {
    /// The next panel in the tab cycle (excluding the modal palette).
    fn next(self) -> Focus {
        match self {
            Focus::Explorer => Focus::Editor,
            Focus::Editor => Focus::Problems,
            Focus::Problems => Focus::Chat,
            Focus::Chat => Focus::Explorer,
            // Tabbing out of the palette returns to the editor.
            Focus::Palette => Focus::Editor,
        }
    }
}

/// The whole IDE shell's view state.
#[derive(Debug)]
pub struct App {
    /// The file explorer (populated once a workspace root is opened).
    pub explorer: Option<Explorer>,
    /// Open editor buffers.
    pub documents: Vec<Document>,
    /// Index of the active buffer.
    pub active: usize,
    /// The command palette overlay.
    pub palette: CommandPalette,
    /// The problems panel.
    pub problems: ProblemsPanel,
    /// The chat / agent-console transcript.
    pub chat: Vec<String>,
    /// The status-bar message.
    pub status: String,
    focus: Focus,
}

impl Default for App {
    fn default() -> Self {
        Self {
            explorer: None,
            documents: Vec::new(),
            active: 0,
            palette: CommandPalette::new(),
            problems: ProblemsPanel::new(),
            chat: Vec::new(),
            status: "ready".into(),
            focus: Focus::Editor,
        }
    }
}

impl App {
    /// Create an empty shell.
    pub fn new() -> Self {
        Self::default()
    }

    /// Populate the command palette from the kernel's command names.
    pub fn set_commands(&mut self, commands: Vec<String>) {
        self.palette.set_commands(commands);
    }

    /// Open a workspace root, scanning it into the explorer.
    pub fn open_workspace(&mut self, root: impl Into<PathBuf>) {
        self.explorer = Some(Explorer::scan(root.into()));
    }

    /// Open a document, making it active.
    pub fn open_document(&mut self, path: Option<PathBuf>, text: &str) {
        self.documents.push(Document::from_str(path, text));
        self.active = self.documents.len() - 1;
    }

    /// The active document, if any.
    pub fn active_document(&self) -> Option<&Document> {
        self.documents.get(self.active)
    }

    /// The active document mutably, if any.
    pub fn active_document_mut(&mut self) -> Option<&mut Document> {
        self.documents.get_mut(self.active)
    }

    /// The focused panel (the palette wins while open).
    pub fn focus(&self) -> Focus {
        if self.palette.is_open() {
            Focus::Palette
        } else {
            self.focus
        }
    }

    /// Set focus explicitly.
    pub fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
    }

    /// Advance focus to the next panel (Tab).
    pub fn cycle_focus(&mut self) {
        self.focus = self.focus().next();
    }

    /// Toggle the command palette overlay.
    pub fn toggle_palette(&mut self) {
        if self.palette.is_open() {
            self.palette.close();
        } else {
            self.palette.open();
        }
    }

    /// Append a line to the chat / agent console.
    pub fn push_chat(&mut self, line: impl Into<String>) {
        self.chat.push(line.into());
    }

    /// Apply a kernel event, routing it to the right view-model. This is the
    /// single seam through which bus traffic mutates UI state.
    pub fn apply_event(&mut self, event: &Event) {
        let topic = event.topic.as_str();
        match topic {
            "lsp.diagnostics" => self.problems.apply(&event.payload),
            "symbols.updated" => {
                let file = event
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or("?");
                let count = event
                    .payload
                    .get("count")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0);
                self.status = format!("indexed {count} symbols in {file}");
            }
            "fs.changed" => {
                if let Some(path) = event.payload.get("path").and_then(|p| p.as_str()) {
                    self.status = format!("changed: {path}");
                }
            }
            t if t.starts_with("agent.") || t.starts_with("skill.") => {
                self.chat
                    .push(format!("[{}] {}", t, compact(&event.payload)));
            }
            _ => {}
        }
    }
}

/// Render a JSON payload as a compact single line for the chat log.
fn compact(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_cycles_through_panels() {
        let mut app = App::new();
        assert_eq!(app.focus(), Focus::Editor);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Problems);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Chat);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Explorer);
    }

    #[test]
    fn palette_takes_focus_while_open() {
        let mut app = App::new();
        app.toggle_palette();
        assert_eq!(app.focus(), Focus::Palette);
        app.toggle_palette();
        assert_ne!(app.focus(), Focus::Palette);
    }

    #[test]
    fn agent_events_land_in_chat() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "agent.status",
            serde_json::json!({ "status": "running" }),
        ));
        assert_eq!(app.chat.len(), 1);
        assert!(app.chat[0].contains("running"));
    }

    #[test]
    fn skill_events_land_in_chat() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "skill.equipped",
            serde_json::json!({ "skill": "code-review" }),
        ));
        assert_eq!(app.chat.len(), 1);
        assert!(app.chat[0].contains("code-review"));
    }

    #[test]
    fn diagnostics_event_updates_problems() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "lsp.diagnostics",
            serde_json::json!({
                "uri": "file:///a.rs",
                "diagnostics": [{ "range": { "start": { "line": 1 } }, "severity": "error", "message": "boom" }]
            }),
        ));
        assert_eq!(app.problems.count(), 1);
        assert_eq!(app.problems.all()[0].message, "boom");
    }

    #[test]
    fn symbols_event_updates_status() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "symbols.updated",
            serde_json::json!({ "file": "a.rs", "count": 7 }),
        ));
        assert!(app.status.contains('7'));
    }
}
