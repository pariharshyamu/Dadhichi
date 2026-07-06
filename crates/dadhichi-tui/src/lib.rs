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

    // The right column holds the Agent Console, with a live Plan checklist
    // stacked above it once the agent has produced a plan.
    if app.plan.is_some() {
        let right = Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(cols[2]);
        render_plan(app, frame, right[0]);
        render_chat(app, frame, right[1]);
    } else {
        render_chat(app, frame, cols[2]);
    }
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

fn render_plan(app: &App, frame: &mut Frame, area: Rect) {
    let Some(plan) = app.plan.as_ref() else {
        return;
    };
    let heading = format!("Plan ({}%)", plan.percent_done());
    let block = panel(&heading, false);

    // The first not-yet-done step is the "active" one — mark it distinctly so the
    // eye lands on what the agent is working on now.
    let mut active_marked = false;
    let items: Vec<ListItem> = plan
        .steps
        .iter()
        .map(|step| {
            if step.done {
                ListItem::new(Line::from(vec![
                    Span::styled("☑ ", Style::default().fg(Color::Green)),
                    Span::styled(
                        step.description.clone(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]))
            } else if !active_marked {
                active_marked = true;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        "▸ ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        step.description.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled("☐ ", Style::default().fg(Color::DarkGray)),
                    Span::raw(step.description.clone()),
                ]))
            }
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

fn render_chat(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focus() == Focus::Chat;
    let title = if app.agent_running {
        "Agent Console  ⋯ running"
    } else {
        "Agent Console"
    };
    let block = panel(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split the console: scrolling transcript on top, a one-line goal input
    // pinned to the bottom (with a rule above it).
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    // Keep the newest transcript lines visible by scrolling to the tail.
    let text: Vec<Line> = app.chat.iter().map(|l| Line::from(l.as_str())).collect();
    let overflow = text.len().saturating_sub(rows[0].height as usize) as u16;
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((overflow, 0)),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(rows[1].width as usize),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );

    // The goal input. A dim hint stands in until the user types; once focused a
    // block cursor marks the caret.
    let input = if app.prompt.is_empty() && !focused {
        Line::from(Span::styled(
            "❯ type a goal, press Enter to run · Tab to switch panes",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        let caret = if focused { "▏" } else { "" };
        // Keep the caret in view: when the goal outgrows the line, show its tail.
        let budget = (rows[2].width as usize).saturating_sub(3);
        let shown: String = {
            let chars: Vec<char> = app.prompt.chars().collect();
            let start = chars.len().saturating_sub(budget);
            chars[start..].iter().collect()
        };
        Line::from(vec![
            Span::styled(
                "❯ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(shown),
            Span::styled(caret, Style::default().fg(Color::Green)),
        ])
    };
    frame.render_widget(Paragraph::new(input), rows[2]);
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
        Span::raw("  Enter run goal · Ctrl-P palette · Tab focus · Ctrl-Q quit"),
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
    fn renders_plan_panel_when_a_plan_is_present() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.apply_event(&dadhichi_core::Event::new(
            "agent.plan",
            serde_json::json!({
                "goal": "g",
                "steps": [
                    { "description": "first thing", "done": true },
                    { "description": "second thing", "done": false }
                ]
            }),
        ));
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Plan (50%)"), "plan header with progress");
        assert!(text.contains("first thing"), "completed step shown");
        assert!(text.contains("second thing"), "pending step shown");
    }

    #[test]
    fn renders_goal_input_line() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app(); // starts focused on the console
        app.prompt_push('f');
        app.prompt_push('i');
        app.prompt_push('x');
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("❯ fix"), "typed goal shown in the input line");
    }

    #[test]
    fn renders_goal_hint_when_empty_and_unfocused() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.set_focus(Focus::Editor); // move focus off the console
        terminal.draw(|f| render(&app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("type a goal"),
            "hint shown when input is empty"
        );
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
