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
    app.apply_event(&dadhichi_core::Event::new(
        "agent.status",
        serde_json::json!({ "status": "completed", "confidence": 0.9 }),
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
            row.push_str(buffer[(x, y)].symbol());
        }
        println!("│{row}│");
    }
    println!("└{border}┘");
}
