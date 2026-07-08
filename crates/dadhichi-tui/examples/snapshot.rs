//! Renders the Dadhichi shell to an in-memory buffer and prints it, so the
//! layout can be seen without a TTY. Run with:
//! `cargo run -p dadhichi-tui --example snapshot`.

use dadhichi_ui::App;
use ratatui::{Terminal, backend::TestBackend};

fn main() {
    let mut app = App::new();
    app.open_document(
        Some("crates/dadhichi-core/src/lib.rs".into()),
        "pub struct Kernel {\n    bus: EventBus,\n    commands: CommandRegistry,\n}\n\nimpl Kernel {\n    pub fn new() -> Self { .. }\n}\n",
    );
    app.set_commands(vec![
        "agent.run".into(),
        "editor.format".into(),
        "editor.save".into(),
        "git.commit".into(),
        "workspace.reindex".into(),
    ]);
    app.apply_event(&dadhichi_core::Event::new(
        "lsp.diagnostics",
        serde_json::json!({
            "uri": "file:///crates/dadhichi-core/src/lib.rs",
            "diagnostics": [
                { "range": { "start": { "line": 6 } }, "severity": "warning", "message": "unfinished function body" }
            ]
        }),
    ));
    app.apply_event(&dadhichi_core::Event::new(
        "agent.status",
        serde_json::json!({ "status": "planning" }),
    ));
    app.apply_event(&dadhichi_core::Event::new(
        "agent.plan",
        serde_json::json!({
            "goal": "add a retry to the http client",
            "steps": [
                { "description": "analyse the request", "done": true },
                { "description": "design the implementation", "done": true },
                { "description": "write the code", "done": false },
                { "description": "self-review", "done": false }
            ]
        }),
    ));
    // Leave the run mid-flight (a tool call in progress) so the console shows its
    // animated "running" indicator in this snapshot.
    app.apply_event(&dadhichi_core::Event::new(
        "agent.tool",
        serde_json::json!({ "tool": "fs.read", "args": { "path": "src/http.rs" } }),
    ));
    // Context compaction fired mid-run: it logs a concise console line.
    app.apply_event(&dadhichi_core::Event::new(
        "agent.compacted",
        serde_json::json!({ "before_tokens": 178_320, "after_tokens": 4_120, "threshold": 170_000 }),
    ));
    // A gated shell command is paused for approval: the input line becomes a
    // y/n prompt until the user answers `y` or `n`.
    app.apply_event(&dadhichi_core::Event::new(
        "agent.approval",
        serde_json::json!({
            "id": "demo",
            "tool": "terminal.run",
            "permission": "run_commands",
            "summary": "terminal.run (run_commands): {\"command\":\"cargo test --workspace\"}"
        }),
    ));
    app.status = "on branch claude/agentic-ide-rust-ctgpcx".into();

    let (w, h) = (100u16, 26u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| dadhichi_tui::render(&mut app, f))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let border = "─".repeat(w as usize);
    println!("┌{border}┐");
    for y in 0..h {
        let mut row = String::new();
        for x in 0..w {
            let cell = &buffer[(x, y)];
            // Emit ANSI so the colour work (syntax, selection, phase) is visible
            // when this snapshot is printed to a real terminal.
            row.push_str(&ansi(cell));
        }
        // Reset at end of line so colour never bleeds past the frame.
        println!("│{row}\x1b[0m│");
    }
    println!("└{border}┘");
}

/// Render a ratatui cell as an ANSI-escaped string (fg colour + bold).
fn ansi(cell: &ratatui::buffer::Cell) -> String {
    use ratatui::style::{Color, Modifier};
    let mut codes: Vec<String> = Vec::new();
    if cell.modifier.contains(Modifier::BOLD) {
        codes.push("1".into());
    }
    let fg = match cell.fg {
        Color::Reset => None,
        Color::Black => Some(30),
        Color::Red => Some(31),
        Color::Green => Some(32),
        Color::Yellow => Some(33),
        Color::Blue => Some(34),
        Color::Magenta => Some(35),
        Color::Cyan => Some(36),
        Color::Gray => Some(37),
        Color::DarkGray => Some(90),
        Color::White => Some(97),
        _ => None,
    };
    if let Some(code) = fg {
        codes.push(code.to_string());
    }
    let bg = match cell.bg {
        Color::Green => Some(42),
        Color::DarkGray => Some(100),
        Color::Yellow => Some(43),
        _ => None,
    };
    if let Some(code) = bg {
        codes.push(code.to_string());
    }
    if codes.is_empty() {
        cell.symbol().to_string()
    } else {
        format!("\x1b[{}m{}\x1b[0m", codes.join(";"), cell.symbol())
    }
}
