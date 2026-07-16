//! Interactive entry point for the Dadhichi terminal shell.
//!
//! It boots an [`AppController`] — the live kernel, agents, and indexer — then
//! runs a `crossterm` loop that pumps bus events into the UI each frame and
//! translates keystrokes into view-model updates and command dispatches. The
//! render path and view-models are unit-tested in the library; this file is the
//! thin, TTY-bound driver.

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dadhichi_app::{AppController, Decision};
use dadhichi_git::GitRepo;
use dadhichi_ui::Focus;
use ratatui::{Terminal, backend::CrosstermBackend};

#[tokio::main]
async fn main() -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut controller = AppController::new(&cwd).await;

    // Seed the editor with a file and the status bar with the git branch.
    if let Ok(text) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
        controller
            .ui_mut()
            .open_document(Some(cwd.join("Cargo.toml")), &text);
    }
    controller.ui_mut().status = match GitRepo::open(&cwd).ok().and_then(|r| r.current_branch()) {
        Some(branch) => format!("on branch {branch}"),
        None => "no git repository".into(),
    };
    // Greet in the console so the goal input isn't facing an empty void.
    controller
        .ui_mut()
        .push_chat("Welcome to Dadhichi. Type a goal below and press Enter.");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_loop(&mut terminal, &mut controller).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    controller: &mut AppController,
) -> io::Result<()> {
    loop {
        // Drain live bus events (agent progress, diagnostics, indexing) into the
        // view-models, advance the animation frame, then draw.
        controller.pump();
        controller.ui_mut().tick();
        terminal.draw(|f| dadhichi_tui::render(controller.ui_mut(), f))?;

        // Poll faster while the agent is active so the spinner animates smoothly;
        // fall back to a lazy tick when idle to keep the app near-zero-CPU.
        let poll = if controller.ui().agent_phase().is_active() {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll)?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if handle_key(controller, key.code, key.modifiers).await {
                return Ok(());
            }
        }
    }
}

/// Handle one keypress. Returns `true` when the app should quit.
async fn handle_key(ctrl: &mut AppController, code: KeyCode, mods: KeyModifiers) -> bool {
    if ctrl.ui().palette.is_open() {
        match code {
            KeyCode::Esc => ctrl.ui_mut().palette.close(),
            KeyCode::Enter => {
                // Dispatch the selected command through the kernel; its events
                // stream back into the panels on the next pump.
                ctrl.run_palette_selection().await;
            }
            KeyCode::Backspace => ctrl.ui_mut().palette.backspace(),
            KeyCode::Up => ctrl.ui_mut().palette.select_prev(),
            KeyCode::Down => ctrl.ui_mut().palette.select_next(),
            KeyCode::Char(c) => ctrl.ui_mut().palette.push(c),
            _ => {}
        }
        return false;
    }

    // Ctrl-P and quit are global; Esc quits only outside the console, where it
    // would otherwise swallow a keystroke the goal input wants.
    match (code, mods) {
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            ctrl.ui_mut().toggle_palette();
            return false;
        }
        (KeyCode::Char('q'), KeyModifiers::CONTROL) => return true,
        // Ctrl-B collapses/expands the Explorer to reclaim its width.
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
            ctrl.ui_mut().toggle_explorer();
            return false;
        }
        // Ctrl-S saves the active editor buffer to disk.
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            if let Err(err) = ctrl.save_active_document() {
                ctrl.ui_mut().status = format!("save failed: {err}");
            }
            return false;
        }
        // Ctrl-F opens the editor's incremental find line.
        (KeyCode::Char('f'), KeyModifiers::CONTROL) if ctrl.ui().focus() == Focus::Editor => {
            ctrl.ui_mut().find_begin();
            ctrl.ui_mut().status = "/".into();
            return false;
        }
        _ => {}
    }

    // While the find line is open it captures every keystroke: type to edit the
    // query, Enter jumps to the next match (repeat to walk matches), Esc closes.
    if ctrl.ui().is_finding() {
        match code {
            KeyCode::Enter => {
                ctrl.ui_mut().find_run(true);
            }
            KeyCode::Esc => {
                ctrl.ui_mut().find_close();
                ctrl.ui_mut().status = "ready".into();
            }
            KeyCode::Backspace => {
                ctrl.ui_mut().find_backspace();
                let q = ctrl.ui().find_query().to_string();
                ctrl.ui_mut().status = format!("/{q}");
            }
            KeyCode::Char(c) => {
                ctrl.ui_mut().find_push(c);
                let q = ctrl.ui().find_query().to_string();
                ctrl.ui_mut().status = format!("/{q}");
            }
            _ => {}
        }
        return false;
    }

    // A pending tool-approval prompt captures the next keystroke globally: `y`
    // approves the parked call, `n`/Esc rejects it. Nothing else is dispatched
    // until it's answered, so a shell command can't slip past the gate.
    if ctrl.ui().pending_approval().is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(ctrl, Decision::Approve),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_approval(ctrl, Decision::Deny)
            }
            _ => {}
        }
        return false;
    }

    // A delegation Review panel captures the next keystroke: `y` lands the staged
    // work (flush + commit to the branch), `n`/Esc discards it. Nothing else is
    // dispatched until it's answered.
    if ctrl.ui().pending_delegation_review().is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                ctrl.resolve_delegation(true);
                ctrl.ui_mut().clear_delegation_review();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                ctrl.resolve_delegation(false);
                ctrl.ui_mut().clear_delegation_review();
            }
            _ => {}
        }
        return false;
    }

    // The agent console owns free text: letters build the goal, Enter runs it.
    // PageUp/PageDown (and Ctrl-Up/Down for line-at-a-time) scroll the transcript
    // back through history; new output snaps it back to the tail.
    if ctrl.ui().focus() == Focus::Chat {
        match (code, mods) {
            (KeyCode::PageUp, _) => ctrl.ui_mut().chat_scroll_up(10),
            (KeyCode::PageDown, _) => ctrl.ui_mut().chat_scroll_down(10),
            (KeyCode::Up, KeyModifiers::CONTROL) => ctrl.ui_mut().chat_scroll_up(1),
            (KeyCode::Down, KeyModifiers::CONTROL) => ctrl.ui_mut().chat_scroll_down(1),
            (KeyCode::Enter, _) => submit_goal(ctrl),
            (KeyCode::Backspace, _) => ctrl.ui_mut().prompt_backspace(),
            (KeyCode::Tab, _) => ctrl.ui_mut().cycle_focus(),
            (KeyCode::Char(c), _) => ctrl.ui_mut().prompt_push(c),
            _ => {}
        }
        return false;
    }

    // The editor owns text input. It's handled before the generic navigation
    // match so ordinary keys (`q`, Esc) act on the buffer instead of quitting:
    // typing inserts, Shift+arrows/Home/End extend a selection, plain arrows
    // move and collapse it, and the usual editor shortcuts apply — Ctrl-Z/Y
    // undo/redo, Ctrl-X/C/V cut/copy/paste, Ctrl-A select-all. (Ctrl-S save and
    // Ctrl-F find are handled globally above.)
    if ctrl.ui().focus() == Focus::Editor {
        let shift = mods.contains(KeyModifiers::SHIFT);
        match (code, mods) {
            (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.undo();
                }
            }
            (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.redo();
                }
            }
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.select_all();
                }
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                ctrl.ui_mut().copy_selection();
            }
            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                ctrl.ui_mut().cut_selection();
            }
            (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                ctrl.ui_mut().paste();
            }
            (KeyCode::Tab, _) => ctrl.ui_mut().cycle_focus(),
            (KeyCode::Left, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_left();
                    } else {
                        doc.move_left();
                    }
                }
            }
            (KeyCode::Right, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_right();
                    } else {
                        doc.move_right();
                    }
                }
            }
            (KeyCode::Up, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_up();
                    } else {
                        doc.move_up();
                    }
                }
            }
            (KeyCode::Down, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_down();
                    } else {
                        doc.move_down();
                    }
                }
            }
            (KeyCode::Home, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_line_start();
                    } else {
                        doc.move_line_start();
                    }
                }
            }
            (KeyCode::End, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    if shift {
                        doc.select_line_end();
                    } else {
                        doc.move_line_end();
                    }
                }
            }
            (KeyCode::Backspace, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.backspace();
                }
            }
            (KeyCode::Delete, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.delete();
                }
            }
            (KeyCode::Enter, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.insert("\n");
                }
            }
            // Esc clears the selection rather than quitting, so a stray Esc in
            // the editor doesn't tear down the app.
            (KeyCode::Esc, _) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.clear_selection();
                }
            }
            // Any printable key (including Shift-produced capitals) inserts,
            // unless Ctrl is held — a Ctrl combo we don't bind is ignored.
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                if let Some(doc) = ctrl.ui_mut().active_document_mut() {
                    doc.insert(&c.to_string());
                }
            }
            _ => {}
        }
        return false;
    }

    match (code, mods) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return true,
        (KeyCode::Tab, _) => ctrl.ui_mut().cycle_focus(),
        (KeyCode::Up, _) => navigate(ctrl, -1),
        (KeyCode::Down, _) => navigate(ctrl, 1),
        (KeyCode::Enter, _) => activate(ctrl),
        _ => {}
    }
    false
}

/// Submit the typed goal to the agent. Echoes it into the transcript and starts
/// the run **without blocking** — `start_agent_goal` spawns the dispatch, so the
/// render loop keeps pumping and the run's `agent.*` events stream into the
/// console as the model produces them. Marks the console busy until a terminal
/// `agent.*` event clears it.
fn submit_goal(ctrl: &mut AppController) {
    let Some(goal) = ctrl.ui_mut().take_prompt() else {
        return;
    };
    // A leading `!` runs the rest as a shell command through the gated
    // `terminal.run` tool — the approval prompt fires before it executes.
    if let Some(command) = goal.strip_prefix('!') {
        let command = command.trim().to_string();
        if command.is_empty() {
            return;
        }
        ctrl.ui_mut().push_chat(format!("$ {command}"));
        ctrl.start_terminal(&command);
        return;
    }
    // A leading `@` delegates the rest to a specialist: `@code-agent add a test`.
    // It works in an isolated overlay and lands on the branch after the critic
    // verifies it (or you approve it in the Review panel).
    if let Some(rest) = goal.strip_prefix('@') {
        let rest = rest.trim();
        let (subagent, task) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let task = task.trim();
        if task.is_empty() {
            ctrl.ui_mut()
                .push_chat("usage: @<specialist> <task>".to_string());
            return;
        }
        ctrl.ui_mut()
            .push_chat(format!("⇥ delegate {subagent}: {task}"));
        ctrl.start_delegation(subagent, task);
        return;
    }
    ctrl.ui_mut().push_chat(format!("❯ {goal}"));
    ctrl.ui_mut().set_agent_running(true);
    ctrl.start_agent_goal(&goal);
}

/// Answer the pending approval prompt and clear it from the view. The parked
/// tool call resumes (or is rejected) on the controller's one-shot channel.
fn resolve_approval(ctrl: &mut AppController, decision: Decision) {
    if let Some(id) = ctrl.ui().pending_approval().map(|p| p.id.clone()) {
        ctrl.resolve_approval(&id, decision);
        ctrl.ui_mut().clear_approval();
    }
}

/// Move the selection/cursor within the focused panel.
fn navigate(ctrl: &mut AppController, delta: i32) {
    let app = ctrl.ui_mut();
    match app.focus() {
        Focus::Explorer => {
            if let Some(explorer) = app.explorer.as_mut() {
                if delta < 0 {
                    explorer.select_prev();
                } else {
                    explorer.select_next();
                }
            }
        }
        Focus::Problems => {
            if delta < 0 {
                app.problems.select_prev();
            } else {
                app.problems.select_next();
            }
        }
        Focus::Editor => {
            if let Some(doc) = app.active_document_mut() {
                if delta < 0 {
                    doc.move_up();
                } else {
                    doc.move_down();
                }
            }
        }
        _ => {}
    }
}

/// Activate the selection in the focused panel (Enter). In the Explorer a
/// directory expands/collapses; a file is read from disk and opened in the
/// editor, and focus moves there so it can be scrolled and edited immediately.
fn activate(ctrl: &mut AppController) {
    if ctrl.ui().focus() != Focus::Explorer {
        return;
    }
    let selected = ctrl.ui().explorer.as_ref().and_then(|e| {
        let idx = e.selected_index();
        e.rows().into_iter().nth(idx)
    });
    let Some(row) = selected else {
        return;
    };
    if row.is_dir {
        if let Some(explorer) = ctrl.ui_mut().explorer.as_mut() {
            explorer.toggle_selected();
        }
        return;
    }
    match std::fs::read_to_string(&row.path) {
        Ok(text) => {
            ctrl.ui_mut().open_document(Some(row.path.clone()), &text);
            ctrl.ui_mut().set_focus(Focus::Editor);
        }
        Err(err) => {
            ctrl.ui_mut()
                .push_chat(format!("cannot open {}: {err}", row.path.display()));
        }
    }
}
