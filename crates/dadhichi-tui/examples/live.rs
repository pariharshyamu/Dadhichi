//! Boots the live [`AppController`], dispatches an agent run through the kernel
//! command layer (as the command palette would), pumps the resulting bus events
//! into the panels, and prints the rendered frame — proving the whole stack is
//! wired together. Run with: `cargo run -p dadhichi-tui --example live`.

use dadhichi_app::AppController;
use ratatui::{Terminal, backend::TestBackend};

#[tokio::main]
async fn main() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut controller = AppController::new(&cwd).await;

    if let Ok(text) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
        controller
            .ui_mut()
            .open_document(Some("Cargo.toml".into()), &text);
    }

    // Dispatch through the kernel — exactly what selecting the command in the
    // palette does. The agent's progress streams back over the event bus.
    let _ = controller
        .dispatch(
            "agent.run",
            serde_json::json!({ "goal": "Review the workspace architecture", "agent": "review-agent" }),
        )
        .await;
    controller.pump();
    controller.ui_mut().status = "review-agent dispatched via command palette".into();

    let (w, h) = (100u16, 24u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| dadhichi_tui::render(controller.ui(), f))
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
