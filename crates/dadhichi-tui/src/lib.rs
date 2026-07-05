//! # dadhichi-tui
//!
//! A terminal frontend that renders the toolkit-agnostic
//! [`App`](dadhichi_ui::App) with [`ratatui`]. It stands in for the eventual
//! GPU shell: because [`render`] draws from the shared view-models and holds no
//! state of its own, a `wgpu`/GPUI renderer would consume exactly the same
//! `App`. Rendering to `ratatui`'s `TestBackend` makes the whole layout
//! verifiable headlessly.

use dadhichi_ui::{App, Focus};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

/// Draw the entire IDE shell for `app` into `frame`.
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();
    let root = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let body = root[0];
    let status = root[1];

    let cols = Layout::horizontal([
        Constraint::Percentage(22),
        Constraint::Percentage(50),
        Constraint::Percentage(28),
    ])
    .split(body);

    render_explorer(app, frame, cols[0]);

    let center =
        Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)]).split(cols[1]);
    render_editor(app, frame, center[0]);
    render_problems(app, frame, center[1]);

    render_chat(app, frame, cols[2]);
    render_status(app, frame, status);

    if app.palette.is_open() {
        render_palette(app, frame, area);
    }
}

/// A bordered block whose title highlights when `focused`.
fn panel(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style)
}

fn render_explorer(app: &App, frame: &mut Frame, area: Rect) {
    let block = panel("Explorer", app.focus() == Focus::Explorer);
    let items: Vec<ListItem> = match &app.explorer {
        Some(explorer) => explorer
            .rows()
            .into_iter()
            .map(|row| {
                let prefix = "  ".repeat(row.depth);
                let marker = if row.is_dir {
                    if row.expanded { "▾ " } else { "▸ " }
                } else {
                    "  "
                };
                ListItem::new(format!("{prefix}{marker}{}", row.name))
            })
            .collect(),
        None => vec![ListItem::new("(no workspace open)")],
    };
    frame.render_widget(List::new(items).block(block), area);
}

fn render_editor(app: &App, frame: &mut Frame, area: Rect) {
    let title = app
        .active_document()
        .and_then(|d| d.path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "scratch".into());
    let heading = format!("Editor — {title}");
    let block = panel(&heading, app.focus() == Focus::Editor);

    let lines: Vec<Line> = match app.active_document() {
        Some(doc) => (0..doc.line_count().min(area.height as usize))
            .map(|n| {
                let text = doc.line(n).unwrap_or_default();
                Line::from(vec![
                    Span::styled(
                        format!("{:>4} ", n + 1),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(text),
                ])
            })
            .collect(),
        None => vec![Line::from("(no file open)")],
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_problems(app: &App, frame: &mut Frame, area: Rect) {
    let heading = format!("Problems ({})", app.problems.count());
    let block = panel(&heading, app.focus() == Focus::Problems);
    let items: Vec<ListItem> = app
        .problems
        .all()
        .into_iter()
        .map(|p| {
            let color = match p.severity.as_str() {
                "error" => Color::Red,
                "warning" => Color::Yellow,
                _ => Color::Blue,
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>8} ", p.severity), Style::default().fg(color)),
                Span::raw(format!(
                    "{}:{}  {}",
                    short_file(&p.file),
                    p.line + 1,
                    p.message
                )),
            ]))
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

fn render_chat(app: &App, frame: &mut Frame, area: Rect) {
    let block = panel("Agent Console", app.focus() == Focus::Chat);
    let text: Vec<Line> = app.chat.iter().map(|l| Line::from(l.as_str())).collect();
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_status(app: &App, frame: &mut Frame, area: Rect) {
    let focus = format!("{:?}", app.focus());
    let line = Line::from(vec![
        Span::styled(
            " dadhichi ",
            Style::default().bg(Color::Green).fg(Color::Black),
        ),
        Span::raw(format!(" {} ", app.status)),
        Span::styled(format!("[{focus}]"), Style::default().fg(Color::DarkGray)),
        Span::raw("  Ctrl-P palette · Tab focus · q quit"),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_palette(app: &App, frame: &mut Frame, area: Rect) {
    let popup = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup);

    let skill_mode = app.palette.in_skill_mode();
    let title = if skill_mode {
        "Skills  (Enter to run)"
    } else {
        "Command Palette  (› for skills)"
    };
    let block = panel(title, true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Green)),
            Span::raw(app.palette.query()),
        ])),
        rows[0],
    );

    let selected = app.palette.selected_index();
    let items: Vec<ListItem> = app
        .palette
        .items()
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                Style::default().bg(Color::Green).fg(Color::Black)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(item.label().to_string(), style)];
            if let Some(detail) = item.detail() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    detail.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    frame.render_widget(List::new(items), rows[1]);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    // Bind each `split` result: it returns an `Rc<[Rect]>` whose temporary must
    // outlive the indexing, otherwise the borrow checker rejects it.
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(vertical[1]);
    horizontal[1]
}

fn short_file(uri: &str) -> String {
    uri.rsplit(['/', '\\']).next().unwrap_or(uri).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dadhichi_ui::App;
    use ratatui::{Terminal, backend::TestBackend};

    /// Flatten a rendered buffer into a single string for content assertions.
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn demo_app() -> App {
        let mut app = App::new();
        app.set_commands(vec![
            "agent.run".into(),
            "editor.save".into(),
            "git.commit".into(),
        ]);
        app.open_document(Some("src/main.rs".into()), "fn main() {\n    run();\n}\n");
        app.apply_event(&dadhichi_core::Event::new(
            "lsp.diagnostics",
            serde_json::json!({
                "uri": "file:///src/main.rs",
                "diagnostics": [{ "range": { "start": { "line": 1 } }, "severity": "warning", "message": "unused" }]
            }),
        ));
        app.push_chat("[agent.status] status=completed");
        app
    }

    #[test]
    fn renders_all_panels() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let app = demo_app();
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Explorer"), "explorer panel drawn");
        assert!(text.contains("Editor"), "editor panel drawn");
        assert!(text.contains("Problems (1)"), "problems count shown");
        assert!(text.contains("Agent Console"), "chat panel drawn");
        assert!(text.contains("fn main()"), "editor content shown");
        assert!(text.contains("dadhichi"), "status bar drawn");
    }

    #[test]
    fn renders_palette_overlay_when_open() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.toggle_palette();
        app.palette.push('a');
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Command Palette"), "palette overlay drawn");
        assert!(text.contains("agent.run"), "fuzzy result shown");
    }

    #[test]
    fn renders_skill_mode_with_capability_detail() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.palette.set_skills(vec![dadhichi_ui::SkillEntry {
            name: "code-review".into(),
            description: "Review a change".into(),
            detail: "perms: read_workspace · tools: fs.read".into(),
        }]);
        app.toggle_palette();
        app.palette.push('>'); // enter skill mode
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Skills"), "skill-mode title drawn");
        assert!(text.contains("code-review"), "skill listed");
        assert!(text.contains("read_workspace"), "capability detail shown");
    }
}
