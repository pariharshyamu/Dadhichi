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
pub use palette::{CommandPalette, McpEntry, PaletteAction, PaletteItem, SkillEntry};
pub use problems::ProblemsPanel;

use dadhichi_core::Event;
use std::path::PathBuf;

/// One step of the agent's live plan, as shown in the Plan panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanStep {
    /// The step's imperative description.
    pub description: String,
    /// Whether the agent has completed this step.
    pub done: bool,
}

/// The agent's current plan, rebuilt from each `agent.plan` snapshot event so the
/// TUI can render a live checklist that ticks as the run progresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanView {
    /// The goal the plan pursues.
    pub goal: String,
    /// The ordered steps with their completion state.
    pub steps: Vec<PlanStep>,
}

impl PlanView {
    /// Fraction of steps completed in `0..=100` (an empty plan reports 0).
    pub fn percent_done(&self) -> u16 {
        if self.steps.is_empty() {
            return 0;
        }
        let done = self.steps.iter().filter(|s| s.done).count();
        ((done * 100) / self.steps.len()) as u16
    }
}

/// A pending tool-approval request the user must answer before a gated tool
/// (a shell command, a file write) runs. Raised by an `agent.approval` event and
/// cleared by `agent.approval.resolved`, it drives the console's `y/n` prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalPrompt {
    /// Correlates the answer back to the suspended tool call.
    pub id: String,
    /// The tool awaiting approval, e.g. `"terminal.run"`.
    pub tool: String,
    /// The permission that triggered the interrupt, e.g. `"run_commands"`.
    pub permission: String,
    /// A one-line, secret-free summary of the call (tool, permission, args).
    pub summary: String,
}

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
    /// The goal the user is typing into the agent console's input line, not yet
    /// submitted. `take_prompt` drains it when they press Enter.
    pub prompt: String,
    /// Whether an agent run is in flight — drives the console's busy indicator.
    /// Set when a goal is submitted, cleared by a terminal `agent.*` event.
    pub agent_running: bool,
    /// The agent's current plan, rendered as a live checklist. `None` until the
    /// first `agent.plan` event of a run.
    pub plan: Option<PlanView>,
    /// A tool call awaiting the user's `y/n` approval, if any. `Some` between an
    /// `agent.approval` event and its `agent.approval.resolved`.
    pub approval: Option<ApprovalPrompt>,
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
            prompt: String::new(),
            agent_running: false,
            plan: None,
            approval: None,
            status: "ready".into(),
            // Land on the agent console so the goal input has focus at startup —
            // typing a goal and pressing Enter is the primary action.
            focus: Focus::Chat,
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

    /// Append a character to the agent-console goal input.
    pub fn prompt_push(&mut self, c: char) {
        self.prompt.push(c);
    }

    /// Delete the last character of the goal input.
    pub fn prompt_backspace(&mut self) {
        self.prompt.pop();
    }

    /// Take the typed goal, trimmed, clearing the input. Returns `None` when the
    /// input is blank so the frontend can ignore an empty Enter.
    pub fn take_prompt(&mut self) -> Option<String> {
        let goal = self.prompt.trim().to_string();
        self.prompt.clear();
        if goal.is_empty() { None } else { Some(goal) }
    }

    /// Mark an agent run as in flight (or finished). The frontend sets this when
    /// it starts a run; terminal events clear it via `apply_event`.
    pub fn set_agent_running(&mut self, running: bool) {
        self.agent_running = running;
    }

    /// The tool call currently awaiting the user's `y/n`, if any. The frontend
    /// checks this to intercept the keystroke and render the approval prompt.
    pub fn pending_approval(&self) -> Option<&ApprovalPrompt> {
        self.approval.as_ref()
    }

    /// Clear the pending approval prompt (after the user answers it).
    pub fn clear_approval(&mut self) {
        self.approval = None;
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
            // A full plan snapshot: rebuild the Plan panel and keep the console
            // line concise (the step array would otherwise flood it).
            "agent.plan" => {
                if let Some(steps) = event.payload.get("steps").and_then(|s| s.as_array()) {
                    let goal = event
                        .payload
                        .get("goal")
                        .and_then(|g| g.as_str())
                        .unwrap_or("")
                        .to_string();
                    let steps: Vec<PlanStep> = steps
                        .iter()
                        .map(|s| PlanStep {
                            description: s
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                            done: s.get("done").and_then(|d| d.as_bool()).unwrap_or(false),
                        })
                        .collect();
                    let n = steps.len();
                    let done = steps.iter().filter(|s| s.done).count();
                    self.plan = Some(PlanView { goal, steps });
                    self.chat.push(format!(
                        "[agent.plan] {done}/{n} step{}",
                        if n == 1 { "" } else { "s" }
                    ));
                }
            }
            // A gated tool paused for approval: raise the y/n prompt and note it
            // in the console. The run stays "running" while the user decides.
            "agent.approval" => {
                let get = |k: &str| {
                    event
                        .payload
                        .get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let summary = event
                    .payload
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.chat
                    .push(format!("[agent.approval] approve? {summary}"));
                self.approval = Some(ApprovalPrompt {
                    id: get("id"),
                    tool: get("tool"),
                    permission: get("permission"),
                    summary,
                });
            }
            // The user (or a policy) answered: clear the prompt and log the verdict.
            "agent.approval.resolved" => {
                let decision = event
                    .payload
                    .get("decision")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let id = event.payload.get("id").and_then(|v| v.as_str());
                if id.is_none() || self.approval.as_ref().map(|a| a.id.as_str()) == id {
                    self.approval = None;
                }
                self.chat
                    .push(format!("[agent.approval.resolved] {decision}"));
            }
            t if t.starts_with("agent.")
                || t.starts_with("skill.")
                || t.starts_with("mcp.")
                || t.starts_with("terminal.") =>
            {
                // Clear the busy indicator when a run reaches a terminal state or
                // errors out, so the console stops showing "running".
                if t == "agent.error" {
                    self.agent_running = false;
                } else if t == "agent.status" {
                    let status = event.payload.get("status").and_then(|s| s.as_str());
                    if matches!(status, Some("completed" | "failed" | "error" | "idle")) {
                        self.agent_running = false;
                    }
                }
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
        // Startup focus is the agent console (the goal input).
        assert_eq!(app.focus(), Focus::Chat);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Explorer);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Editor);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Problems);
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Chat);
    }

    #[test]
    fn take_prompt_trims_and_clears() {
        let mut app = App::new();
        assert_eq!(app.take_prompt(), None);
        app.prompt_push('h');
        app.prompt_push('i');
        assert_eq!(app.take_prompt().as_deref(), Some("hi"));
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn agent_running_clears_on_terminal_event() {
        let mut app = App::new();
        app.set_agent_running(true);
        // A non-terminal status keeps it running.
        app.apply_event(&Event::new(
            "agent.status",
            serde_json::json!({ "status": "running" }),
        ));
        assert!(app.agent_running);
        // A completed status clears it.
        app.apply_event(&Event::new(
            "agent.status",
            serde_json::json!({ "status": "completed" }),
        ));
        assert!(!app.agent_running);
    }

    #[test]
    fn agent_plan_event_builds_the_plan_panel() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "agent.plan",
            serde_json::json!({
                "goal": "add retries",
                "steps": [
                    { "description": "analyse", "done": true },
                    { "description": "implement", "done": false },
                    { "description": "test", "done": false }
                ]
            }),
        ));
        let plan = app.plan.as_ref().expect("plan built");
        assert_eq!(plan.goal, "add retries");
        assert_eq!(plan.steps.len(), 3);
        assert!(plan.steps[0].done && !plan.steps[1].done);
        assert_eq!(plan.percent_done(), 33);
        // The console got a concise line, not the raw step array.
        assert!(
            app.chat
                .iter()
                .any(|l| l.contains("[agent.plan] 1/3 steps"))
        );
    }

    #[test]
    fn agent_running_clears_on_error() {
        let mut app = App::new();
        app.set_agent_running(true);
        app.apply_event(&Event::new(
            "agent.error",
            serde_json::json!({ "error": "boom" }),
        ));
        assert!(!app.agent_running);
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
