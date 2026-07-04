//! Interactive entry point for the Dadhichi terminal shell.
//!
//! It builds an [`App`] from the current workspace, then runs a `crossterm`
//! event loop translating keystrokes into view-model updates and redrawing via
//! [`dadhichi_tui::render`]. The render path and view-models are unit-tested in
//! the library; this file is the thin, TTY-bound driver.

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dadhichi_git::GitRepo;
use dadhichi_ui::{App, Focus};
use ratatui::{Terminal, backend::CrosstermBackend};

fn main() -> io::Result<()> {
    let mut app = build_app();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Assemble the initial shell state from the current directory.
fn build_app() -> App {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut app = App::new();
    app.open_workspace(&cwd);
    app.set_commands(vec![
        "agent.run".into(),
        "editor.save".into(),
        "editor.format".into(),
        "workspace.reindex".into(),
        "git.commit".into(),
        "git.status".into(),
        "quit".into(),
    ]);

    if let Ok(text) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
        app.open_document(Some(cwd.join("Cargo.toml")), &text);
    }
    app.status = match GitRepo::open(&cwd).ok().and_then(|r| r.current_branch()) {
        Some(branch) => format!("on branch {branch}"),
        None => "no git repository".into(),
    };
    app
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| dadhichi_tui::render(app, f))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if handle_key(app, key.code, key.modifiers) {
                return Ok(());
            }
        }
    }
}

/// Handle one keypress. Returns `true` when the app should quit.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    if app.palette.is_open() {
        match code {
            KeyCode::Esc => app.palette.close(),
            KeyCode::Enter => {
                if let Some(cmd) = app.palette.accept() {
                    app.push_chat(format!("▶ dispatch {cmd}"));
                    app.status = format!("ran {cmd}");
                    app.palette.close();
                    return cmd == "quit";
                }
                app.palette.close();
            }
            KeyCode::Backspace => app.palette.backspace(),
            KeyCode::Up => app.palette.select_prev(),
            KeyCode::Down => app.palette.select_next(),
            KeyCode::Char(c) => app.palette.push(c),
            _ => {}
        }
        return false;
    }

    match (code, mods) {
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => app.toggle_palette(),
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return true,
        (KeyCode::Tab, _) => app.cycle_focus(),
        (KeyCode::Up, _) => navigate(app, -1),
        (KeyCode::Down, _) => navigate(app, 1),
        (KeyCode::Enter, _) => activate(app),
        (KeyCode::Backspace, _) if app.focus() == Focus::Editor => {
            if let Some(doc) = app.active_document_mut() {
                doc.backspace();
            }
        }
        (KeyCode::Char(c), _) if app.focus() == Focus::Editor => {
            if let Some(doc) = app.active_document_mut() {
                doc.insert(&c.to_string());
            }
        }
        _ => {}
    }
    false
}

/// Move the selection/cursor within the focused panel.
fn navigate(app: &mut App, delta: i32) {
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

/// Activate the selection in the focused panel (Enter).
fn activate(app: &mut App) {
    if app.focus() == Focus::Explorer
        && let Some(explorer) = app.explorer.as_mut()
    {
        explorer.toggle_selected();
    }
}
