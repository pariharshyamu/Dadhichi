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

/// A finished delegation whose staged changes await the user's decision before
/// they land on the branch. Raised by an `agent.delegation.review` event when a
/// delegate's work does not clear the critic's confidence threshold, and cleared
/// by `agent.delegation.resolved`. Drives the Review panel's `y/n` prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegationReviewPrompt {
    /// The specialist whose work is under review, e.g. `"code-agent"`.
    pub subagent: String,
    /// The one-line verdict (status, critic confidence vs threshold, notes).
    pub verdict: String,
    /// The files the delegate staged: `(path, is_deletion)`.
    pub files: Vec<(String, bool)>,
}

/// The editor's incremental-find state: the query being typed and whether the
/// find input line is currently capturing keystrokes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindState {
    /// The current search query.
    pub query: String,
    /// Whether the find input line is active (capturing typed characters).
    pub active: bool,
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

/// What the agent is doing right now, driving the animated status indicator in
/// the Agent Console. Derived from the run's `agent.*` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentPhase {
    /// No run in flight — the console is idle.
    #[default]
    Idle,
    /// Planning / consulting the model (between `planning` and the first tool).
    Thinking,
    /// Actively running: a tool call is in flight.
    Running,
    /// A sub-agent has been delegated / spawned and is working in isolation.
    Spawned,
}

impl AgentPhase {
    /// A short label for the status line, e.g. `thinking`.
    pub fn label(self) -> &'static str {
        match self {
            AgentPhase::Idle => "idle",
            AgentPhase::Thinking => "thinking",
            AgentPhase::Running => "running",
            AgentPhase::Spawned => "delegated",
        }
    }

    /// Whether a run is active (any non-idle phase), so the frontend knows to
    /// keep animating and redrawing.
    pub fn is_active(self) -> bool {
        !matches!(self, AgentPhase::Idle)
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
    /// A delegated sub-agent's staged work awaiting the user's decision to land
    /// it on the branch. `Some` between an `agent.delegation.review` event and
    /// its `agent.delegation.resolved`.
    pub delegation_review: Option<DelegationReviewPrompt>,
    /// The status-bar message.
    pub status: String,
    /// The editor's incremental-find state.
    pub find: FindState,
    /// How many lines the agent-console transcript is scrolled up from the tail.
    /// `0` pins to the newest line (the default); a positive value scrolls back
    /// into history. Any new chat line resets it to `0` so live output follows.
    chat_scroll: usize,
    /// Whether the file Explorer pane is shown. Hiding it (Ctrl-B) gives the
    /// editor and agent console the full width — useful on narrow terminals.
    explorer_visible: bool,
    /// What the agent is currently doing, driving the animated console indicator.
    agent_phase: AgentPhase,
    /// A monotonically increasing frame counter the frontend advances each redraw
    /// (via [`App::tick`]) to animate the spinner without any wall-clock state.
    anim_frame: u64,
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
            delegation_review: None,
            status: "ready".into(),
            find: FindState::default(),
            chat_scroll: 0,
            explorer_visible: true,
            agent_phase: AgentPhase::Idle,
            anim_frame: 0,
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

    /// Advance focus to the next panel (Tab). Skips the Explorer while it's
    /// hidden so Tab never lands on an invisible pane.
    pub fn cycle_focus(&mut self) {
        let mut next = self.focus().next();
        if next == Focus::Explorer && !self.explorer_visible {
            next = next.next();
        }
        self.focus = next;
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
    ///
    /// Uses **sticky scroll**: if the user is pinned to the tail
    /// (`chat_scroll == 0`) the view follows the new line; but if they have
    /// scrolled up to read history, the offset is bumped so their viewport stays
    /// anchored on the same content instead of being yanked back to the bottom on
    /// every streamed agent event.
    pub fn push_chat(&mut self, line: impl Into<String>) {
        self.chat.push(line.into());
        if self.chat_scroll > 0 {
            self.chat_scroll += 1;
        }
    }

    /// How far the console transcript is scrolled back from the newest line.
    /// `0` means pinned to the tail (following live output).
    pub fn chat_scroll(&self) -> usize {
        self.chat_scroll
    }

    /// Scroll the console transcript up (into older history) by `lines`, saturating
    /// at the oldest line so it can't scroll into emptiness.
    pub fn chat_scroll_up(&mut self, lines: usize) {
        let max = self.chat.len().saturating_sub(1);
        self.chat_scroll = (self.chat_scroll + lines).min(max);
    }

    /// Scroll the console transcript down (toward the newest line) by `lines`.
    /// Reaching `0` re-pins it to the tail.
    pub fn chat_scroll_down(&mut self, lines: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(lines);
    }

    /// Whether the file Explorer pane is currently shown.
    pub fn explorer_visible(&self) -> bool {
        self.explorer_visible
    }

    /// Show or hide the Explorer pane. When hiding it while it holds focus, move
    /// focus to the editor so keystrokes still land somewhere sensible.
    pub fn toggle_explorer(&mut self) {
        self.explorer_visible = !self.explorer_visible;
        if !self.explorer_visible && self.focus == Focus::Explorer {
            self.focus = Focus::Editor;
        }
    }

    /// The agent's current phase (idle/thinking/running/delegated).
    pub fn agent_phase(&self) -> AgentPhase {
        self.agent_phase
    }

    /// Advance the animation frame counter by one. The frontend calls this once
    /// per redraw so the console spinner animates; it's a no-op semantically
    /// beyond driving [`App::spinner`].
    pub fn tick(&mut self) {
        self.anim_frame = self.anim_frame.wrapping_add(1);
    }

    /// The current spinner glyph for the active phase, cycled by the frame
    /// counter. Braille dots give a smooth spin in a single cell.
    pub fn spinner(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[(self.anim_frame as usize) % FRAMES.len()]
    }

    /// A short animated progress bar (three dots that fill and empty) for the
    /// console header, giving motion even where a spinner is too subtle.
    pub fn pulse(&self) -> &'static str {
        const FRAMES: [&str; 4] = ["·  ", "·· ", "···", " ··"];
        FRAMES[(self.anim_frame as usize / 2) % FRAMES.len()]
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

    /// Whether the editor's find input line is capturing keystrokes.
    pub fn is_finding(&self) -> bool {
        self.find.active
    }

    /// Open the editor find line, starting a fresh query.
    pub fn find_begin(&mut self) {
        self.find.active = true;
        self.find.query.clear();
    }

    /// Append a character to the find query.
    pub fn find_push(&mut self, c: char) {
        self.find.query.push(c);
    }

    /// Delete the last character of the find query.
    pub fn find_backspace(&mut self) {
        self.find.query.pop();
    }

    /// Close the find input line, keeping the query so `n`/`N` can repeat it.
    pub fn find_close(&mut self) {
        self.find.active = false;
    }

    /// The current find query.
    pub fn find_query(&self) -> &str {
        &self.find.query
    }

    /// Search the active buffer for the current find query, moving the cursor to
    /// the match, and report a result message on the status bar. `forward`
    /// chooses direction. Returns whether a match was found.
    pub fn find_run(&mut self, forward: bool) -> bool {
        let query = self.find.query.clone();
        if query.is_empty() {
            return false;
        }
        let found = match self.active_document_mut() {
            Some(doc) if forward => doc.find_next(&query),
            Some(doc) => doc.find_prev(&query),
            None => false,
        };
        self.status = if found {
            format!("/{query}")
        } else {
            format!("/{query} — not found")
        };
        found
    }

    /// Mark an agent run as in flight (or finished). The frontend sets this when
    /// it starts a run; terminal events clear it via `apply_event`. Starting a run
    /// enters the `Thinking` phase so the console animates immediately, before the
    /// first `agent.*` event arrives.
    pub fn set_agent_running(&mut self, running: bool) {
        self.agent_running = running;
        if running {
            self.agent_phase = AgentPhase::Thinking;
        } else {
            self.agent_phase = AgentPhase::Idle;
        }
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

    /// The delegation currently awaiting a land/discard decision, if any. The
    /// frontend checks this to intercept the keystroke and render the Review panel.
    pub fn pending_delegation_review(&self) -> Option<&DelegationReviewPrompt> {
        self.delegation_review.as_ref()
    }

    /// Clear the pending delegation review (after the user decides).
    pub fn clear_delegation_review(&mut self) {
        self.delegation_review = None;
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
            // A delegate's staged work needs a land/discard decision: raise the
            // Review panel with its verdict and change set.
            "agent.delegation.review" => {
                let get = |k: &str| {
                    event
                        .payload
                        .get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let files = event
                    .payload
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|f| {
                                let path = f.get("path")?.as_str()?.to_string();
                                let deleted =
                                    f.get("deleted").and_then(|d| d.as_bool()).unwrap_or(false);
                                Some((path, deleted))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let subagent = get("subagent");
                let verdict = get("verdict");
                self.chat.push(format!(
                    "[agent.delegation.review] {subagent}: {verdict} — land? (y/n)"
                ));
                self.delegation_review = Some(DelegationReviewPrompt {
                    subagent,
                    verdict,
                    files,
                });
            }
            // The delegation decision was made (landed or discarded): clear the
            // panel and log the outcome.
            "agent.delegation.resolved" => {
                let outcome = event
                    .payload
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                self.delegation_review = None;
                self.chat
                    .push(format!("[agent.delegation.resolved] {outcome}"));
            }
            // A delegation landed and committed to the branch.
            "agent.delegation.landed" => {
                let commit = event
                    .payload
                    .get("commit")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let files = event
                    .payload
                    .get("files")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.delegation_review = None;
                self.chat.push(format!(
                    "[agent.delegation.landed] committed {commit} ({files} file(s))"
                ));
            }
            // The model's actual reply. Render it prominently as its own block —
            // a header line then the body split across chat lines so it wraps —
            // rather than as a compacted `key=value` telemetry line.
            "agent.message" => {
                let content = event
                    .payload
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let role = event
                    .payload
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("assistant");
                self.chat.push(format!("‹{role}›"));
                if content.trim().is_empty() {
                    self.chat.push("(empty reply)".to_string());
                } else {
                    for line in content.lines() {
                        self.chat.push(line.to_string());
                    }
                }
            }
            // A tool the agent decided to call: show it as an action line so the
            // user can watch the agent work, not just see a token counter.
            "agent.tool" => {
                self.agent_phase = AgentPhase::Running;
                let tool = event
                    .payload
                    .get("tool")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?");
                let args = event.payload.get("args").map(compact).unwrap_or_default();
                self.chat.push(format!("↳ {tool}({})", truncate(&args, 80)));
            }
            "agent.tool.result" => {
                let result = event.payload.get("result").map(compact).unwrap_or_default();
                self.chat.push(format!("  ✓ {}", truncate(&result, 100)));
            }
            "agent.tool.error" => {
                let err = event
                    .payload
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("");
                self.chat.push(format!("  ✗ {}", truncate(err, 100)));
            }
            t if t.starts_with("agent.")
                || t.starts_with("skill.")
                || t.starts_with("mcp.")
                || t.starts_with("terminal.") =>
            {
                // Clear the busy indicator when a run reaches a terminal state or
                // errors out, so the console stops showing "running". Track the
                // agent phase off the same events so the console can animate it.
                if t == "agent.error" {
                    self.agent_running = false;
                    self.agent_phase = AgentPhase::Idle;
                } else if t == "agent.delegated" {
                    // A sub-agent was spawned to work in isolation.
                    self.agent_phase = AgentPhase::Spawned;
                } else if t == "agent.status" {
                    let status = event.payload.get("status").and_then(|s| s.as_str());
                    match status {
                        Some("completed" | "failed" | "error" | "idle") => {
                            self.agent_running = false;
                            self.agent_phase = AgentPhase::Idle;
                        }
                        // Planning and re-planning between rounds both read as the
                        // Thinking phase (the agent is reasoning, not calling a tool).
                        Some("planning" | "replanning") => {
                            self.agent_phase = AgentPhase::Thinking;
                        }
                        // Don't downgrade Running (a tool call) back to Thinking on
                        // a stray "running" status; only lift Idle up to Thinking.
                        Some("running") if self.agent_phase == AgentPhase::Idle => {
                            self.agent_phase = AgentPhase::Thinking;
                        }
                        _ => {}
                    }
                }
                self.chat
                    .push(format!("[{}] {}", t, compact(&event.payload)));
            }
            _ => {}
        }
    }
}

/// Clip `s` to at most `max` characters, appending an ellipsis when clipped, so
/// a long tool result doesn't flood the console.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("{}…", chars[..max].iter().collect::<String>())
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
    fn tab_skips_the_explorer_while_it_is_hidden() {
        let mut app = App::new(); // focus starts on Chat
        app.toggle_explorer(); // hide the explorer
        assert!(!app.explorer_visible());
        // Chat → (Explorer skipped) → Editor.
        app.cycle_focus();
        assert_eq!(app.focus(), Focus::Editor);
    }

    #[test]
    fn hiding_the_explorer_while_focused_moves_focus_to_the_editor() {
        let mut app = App::new();
        app.set_focus(Focus::Explorer);
        app.toggle_explorer();
        assert!(!app.explorer_visible());
        assert_eq!(app.focus(), Focus::Editor, "focus left the hidden pane");
    }

    #[test]
    fn console_scroll_offset_clamps_and_sticks_while_reading_history() {
        let mut app = App::new();
        for i in 0..5 {
            app.push_chat(format!("l{i}"));
        }
        assert_eq!(app.chat_scroll(), 0, "starts pinned to the tail");

        // Scroll up past the history: it saturates, never exceeding len-1.
        app.chat_scroll_up(100);
        assert_eq!(app.chat_scroll(), 4);

        // Scrolling down returns toward the tail and can't go negative.
        app.chat_scroll_down(2);
        assert_eq!(app.chat_scroll(), 2);
        app.chat_scroll_down(100);
        assert_eq!(app.chat_scroll(), 0);

        // While pinned to the tail, new output keeps following it.
        app.push_chat("newest");
        assert_eq!(app.chat_scroll(), 0, "at the tail, the view follows new output");

        // But once the user scrolls up to read history, streamed lines DON'T yank
        // the view down — the offset grows to keep the same content in view.
        app.chat_scroll_up(3);
        assert_eq!(app.chat_scroll(), 3);
        app.push_chat("streamed-1");
        app.push_chat("streamed-2");
        assert_eq!(app.chat_scroll(), 5, "sticky scroll preserves the read position");
    }

    #[test]
    fn agent_phase_tracks_run_lifecycle_and_animates() {
        let mut app = App::new();
        assert_eq!(app.agent_phase(), AgentPhase::Idle);

        // Starting a run enters Thinking so the console animates immediately.
        app.set_agent_running(true);
        assert_eq!(app.agent_phase(), AgentPhase::Thinking);

        // A planning status keeps it Thinking; a tool call flips to Running.
        app.apply_event(&Event::new(
            "agent.status",
            serde_json::json!({ "status": "planning" }),
        ));
        assert_eq!(app.agent_phase(), AgentPhase::Thinking);
        app.apply_event(&Event::new(
            "agent.tool",
            serde_json::json!({ "tool": "fs.read", "args": {} }),
        ));
        assert_eq!(app.agent_phase(), AgentPhase::Running);

        // A completed status returns to Idle and clears the busy flag.
        app.apply_event(&Event::new(
            "agent.status",
            serde_json::json!({ "status": "completed" }),
        ));
        assert_eq!(app.agent_phase(), AgentPhase::Idle);
        assert!(!app.agent_running);

        // The spinner glyph advances as frames tick, and the phase reports active.
        app.set_agent_running(true);
        assert!(app.agent_phase().is_active());
        let a = app.spinner();
        app.tick();
        let b = app.spinner();
        assert_ne!(a, b, "the spinner advances between frames");
    }

    #[test]
    fn delegation_sets_the_spawned_phase() {
        let mut app = App::new();
        app.set_agent_running(true);
        app.apply_event(&Event::new(
            "agent.delegated",
            serde_json::json!({ "subagent": "test-agent", "task": "cover the parser" }),
        ));
        assert_eq!(app.agent_phase(), AgentPhase::Spawned);
        assert_eq!(app.agent_phase().label(), "delegated");
    }

    #[test]
    fn delegation_review_prompt_is_raised_and_cleared() {
        let mut app = App::new();
        assert!(app.pending_delegation_review().is_none());

        app.apply_event(&dadhichi_core::Event::new(
            "agent.delegation.review",
            serde_json::json!({
                "subagent": "code-agent",
                "verdict": "Completed · critic confidence 40% vs threshold 75%",
                "files": [
                    { "path": "src/new.rs", "deleted": false },
                    { "path": "old.rs", "deleted": true }
                ]
            }),
        ));
        let review = app.pending_delegation_review().expect("review raised");
        assert_eq!(review.subagent, "code-agent");
        assert!(review.verdict.contains("40%"));
        assert_eq!(review.files.len(), 2);
        assert_eq!(review.files[1], ("old.rs".to_string(), true));

        // Resolving clears the panel.
        app.apply_event(&dadhichi_core::Event::new(
            "agent.delegation.resolved",
            serde_json::json!({ "outcome": "discarded" }),
        ));
        assert!(app.pending_delegation_review().is_none());
    }

    #[test]
    fn editor_find_moves_the_cursor_through_matches() {
        let mut app = App::new();
        app.open_document(Some("f.rs".into()), "let x = foo(); // foo again\nfoo");
        // Type a query into the find line.
        app.find_begin();
        assert!(app.is_finding());
        for c in "foo".chars() {
            app.find_push(c);
        }
        assert_eq!(app.find_query(), "foo");

        // Running the search jumps the cursor to the first match (offset 8).
        assert!(app.find_run(true));
        assert_eq!(app.active_document().unwrap().cursor(), 8);
        // Repeating advances to the next occurrence.
        assert!(app.find_run(true));
        assert_eq!(app.active_document().unwrap().cursor(), 18);

        // A miss reports not-found on the status bar.
        app.find.query = "zzz".into();
        assert!(!app.find_run(true));
        assert!(app.status.contains("not found"));

        app.find_close();
        assert!(!app.is_finding());
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
    fn agent_message_event_renders_the_reply_body() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": "line one\nline two" }),
        ));
        // The reply body lands in the console verbatim (header + each line),
        // not as a compacted `content=...` telemetry line.
        assert!(app.chat.iter().any(|l| l.contains("‹assistant›")));
        assert!(app.chat.iter().any(|l| l == "line one"));
        assert!(app.chat.iter().any(|l| l == "line two"));
        assert!(
            !app.chat.iter().any(|l| l.contains("content=")),
            "reply must not be rendered as a telemetry key=value line"
        );
    }

    #[test]
    fn agent_message_event_handles_empty_content() {
        let mut app = App::new();
        app.apply_event(&Event::new(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": "   " }),
        ));
        assert!(app.chat.iter().any(|l| l.contains("(empty reply)")));
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
