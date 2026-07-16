//! # dadhichi-tui
//!
//! A terminal frontend that renders the toolkit-agnostic
//! [`App`](dadhichi_ui::App) with [`ratatui`]. It stands in for the eventual
//! GPU shell: because [`render`] draws from the shared view-models and holds no
//! state of its own, a `wgpu`/GPUI renderer would consume exactly the same
//! `App`. Rendering to `ratatui`'s `TestBackend` makes the whole layout
//! verifiable headlessly.

use dadhichi_ui::{AgentPhase, App, Focus};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

/// Draw the entire IDE shell for `app` into `frame`.
pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let root = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let body = root[0];
    let status = root[1];

    // The Explorer is collapsible (Ctrl-B). When hidden, the editor and console
    // reclaim its width — valuable on narrow terminals. The column indices shift
    // accordingly, so bind them by name rather than a fixed offset.
    let (explorer_col, center_col, right_col) = if app.explorer_visible() {
        let cols = Layout::horizontal([
            Constraint::Percentage(22),
            Constraint::Percentage(50),
            Constraint::Percentage(28),
        ])
        .split(body);
        (Some(cols[0]), cols[1], cols[2])
    } else {
        let cols =
            Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).split(body);
        (None, cols[0], cols[1])
    };

    if let Some(area) = explorer_col {
        render_explorer(app, frame, area);
    }

    let center = Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(center_col);
    // Scroll the active buffer so the cursor stays visible in the editor pane
    // (its inner height is the area minus the top and bottom border rows).
    let editor_rows = center[0].height.saturating_sub(2) as usize;
    if let Some(doc) = app.active_document_mut() {
        doc.ensure_visible(editor_rows);
    }
    render_editor(app, frame, center[0]);
    render_problems(app, frame, center[1]);

    // The right column holds the Agent Console, with a live Plan checklist
    // stacked above it once the agent has produced a plan.
    if app.plan.is_some() {
        let right = Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(right_col);
        render_plan(app, frame, right[0]);
        render_chat(app, frame, right[1]);
    } else {
        render_chat(app, frame, right_col);
    }
    render_status(app, frame, status);

    if app.palette.is_open() {
        render_palette(app, frame, area);
    }
    // The delegation Review panel is a modal that sits above everything else.
    if app.pending_delegation_review().is_some() {
        render_delegation_review(app, frame, area);
    }
}

/// A modal Review panel for a delegated sub-agent's staged work: its verdict and
/// the files it changed, with a `y`/`n` prompt to land it on the branch or
/// discard it.
fn render_delegation_review(app: &App, frame: &mut Frame, area: Rect) {
    let Some(review) = app.pending_delegation_review() else {
        return;
    };
    // A centred box: 70% wide, up to ~60% tall.
    let w = (area.width as f32 * 0.7) as u16;
    let h = ((review.files.len() as u16) + 7).min((area.height as f32 * 0.6) as u16);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect {
        x,
        y,
        width: w.max(20),
        height: h.max(7),
    };
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Delegation Review — {} ", review.subagent))
        .border_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        );

    let mut lines = vec![
        Line::from(Span::styled(
            review.verdict.clone(),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Staged changes:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for (path, deleted) in &review.files {
        let (mark, color) = if *deleted {
            ("D", Color::Red)
        } else {
            ("M", Color::Green)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {mark} "), Style::default().fg(color)),
            Span::raw(path.clone()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Land on the branch?  y = commit   ·   n/Esc = discard",
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        modal,
    );
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
    let focused = app.focus() == Focus::Explorer;
    let block = panel("Explorer", focused);
    let (items, selected): (Vec<ListItem>, Option<usize>) = match &app.explorer {
        Some(explorer) => {
            let items = explorer
                .rows()
                .into_iter()
                .map(|row| {
                    let prefix = "  ".repeat(row.depth);
                    let marker = if row.is_dir {
                        if row.expanded { "▾ " } else { "▸ " }
                    } else {
                        "  "
                    };
                    // Directories in a distinct colour so the tree structure reads
                    // even when nothing is selected.
                    let style = if row.is_dir {
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(Span::styled(format!("{prefix}{marker}{}", row.name), style))
                })
                .collect();
            (items, Some(explorer.selected_index()))
        }
        None => (vec![ListItem::new("(no workspace open)")], None),
    };

    // A stateful List highlights the selected row and scrolls to keep it in view,
    // so navigating a long tree stays visible. The highlight is bright when the
    // pane is focused and muted otherwise, so the selection never disappears.
    let highlight = if focused {
        Style::default()
            .bg(Color::Green)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight)
        .highlight_symbol("▏");
    let mut state = ListState::default();
    state.select(selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_editor(app: &App, frame: &mut Frame, area: Rect) {
    let title = app
        .active_document()
        .and_then(|d| d.path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "scratch".into());
    let heading = format!("Editor — {title}");
    let block = panel(&heading, app.focus() == Focus::Editor);

    // Render the slice of lines currently scrolled into view, numbered by their
    // absolute position so the gutter stays truthful as the buffer scrolls. Lines
    // that carry an LSP diagnostic get a coloured marker in the gutter (● error,
    // ▲ warning) so problems are visible where they occur, not only in the panel.
    let rows = area.height.saturating_sub(2) as usize;
    let doc_path = app
        .active_document()
        .and_then(|d| d.path.as_ref())
        .map(|p| p.display().to_string());
    let lines: Vec<Line> = match app.active_document() {
        Some(doc) => {
            let top = doc.scroll();
            let cursor_line = doc.cursor_line_col().0;
            let selection = doc.selection();
            (top..(top + rows).min(doc.line_count()))
                .map(|n| {
                    let text = doc.line(n).unwrap_or_default();
                    let (marker, marker_style) = match doc_path
                        .as_deref()
                        .and_then(|p| app.problems.severity_on_line(p, n as u32))
                    {
                        Some("error") => ("●", Style::default().fg(Color::Red)),
                        Some("warning") => ("▲", Style::default().fg(Color::Yellow)),
                        Some(_) => ("•", Style::default().fg(Color::Cyan)),
                        None => (" ", Style::default()),
                    };
                    // Highlight the cursor's line so navigation and find jumps are
                    // visible even without a blinking caret.
                    let on_cursor = n == cursor_line;
                    let gutter_style = if on_cursor {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    // Syntax-colour the code so tokens are legible instead of a
                    // flat monochrome wall; the cursor line is additionally bold.
                    let mut spans = vec![
                        Span::styled(marker, marker_style),
                        Span::styled(format!("{:>4} ", n + 1), gutter_style),
                    ];
                    // Overlay the selection: syntax-highlight the parts outside
                    // it and draw the selected span reversed so a multi-line
                    // selection reads clearly across the pane.
                    spans.extend(line_content_spans(
                        &text,
                        selection,
                        doc.line_start(n),
                        on_cursor,
                    ));
                    Line::from(spans)
                })
                .collect()
        }
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
    // The title carries an animated indicator that reflects the agent's phase:
    // a spinning braille glyph plus a phase label (thinking / running / delegated)
    // and a pulsing dot trail, so the console visibly "breathes" while it works.
    let phase = app.agent_phase();
    let title_line = if phase.is_active() {
        let colour = match phase {
            AgentPhase::Thinking => Color::Cyan,
            AgentPhase::Running => Color::Green,
            AgentPhase::Spawned => Color::Magenta,
            AgentPhase::Idle => Color::DarkGray,
        };
        Line::from(vec![
            Span::styled(
                " Agent Console ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                phase.label(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.pulse(), Style::default().fg(colour)),
        ])
    } else {
        Line::from(Span::styled(
            " Agent Console ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };
    let border_style = if focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title_line)
        .border_style(border_style);
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

    // The transcript follows the tail by default, but the user can scroll back
    // through history (PageUp/PageDown when the console is focused). `chat_scroll`
    // is the offset up from the bottom; subtract it from the tail-pinned offset.
    let total = app.chat.len();
    let viewport = rows[0].height as usize;
    let overflowing = total > viewport;

    // When the transcript overflows, reserve the rightmost column for a scrollbar
    // so it never paints over the text; otherwise the text uses the full width.
    let (text_area, scrollbar_area) = if overflowing {
        let split =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(rows[0]);
        (split[0], Some(split[1]))
    } else {
        (rows[0], None)
    };

    let max_offset = total.saturating_sub(viewport);
    let back = app.chat_scroll().min(max_offset);
    let offset = max_offset.saturating_sub(back) as u16;
    // Colour-code each transcript line by its kind so the eye can separate the
    // model's answers from tool activity and dim telemetry at a glance — the
    // legibility fix a terminal can offer in place of a smaller font.
    let text: Vec<Line> = app.chat.iter().map(|l| style_chat_line(l)).collect();
    frame.render_widget(Paragraph::new(text).scroll((offset, 0)), text_area);

    // A scrollbar in the reserved column shows position and that there's more
    // history above/below — drawn only when the content overflows the pane.
    if let Some(sb_area) = scrollbar_area {
        let mut sb_state = ScrollbarState::new(max_offset).position(offset as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"))
                .thumb_symbol("█")
                .track_symbol(Some("│")),
            sb_area,
            &mut sb_state,
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(rows[1].width as usize),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );

    // A pending approval commandeers the input line with a y/n prompt so the
    // choice is impossible to miss and lands right where the eye already is.
    if let Some(prompt) = app.pending_approval() {
        let budget = (rows[2].width as usize).saturating_sub(24);
        let summary: String = prompt.summary.chars().take(budget).collect();
        let line = Line::from(vec![
            Span::styled(
                " APPROVE ",
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {summary} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("[y/n]", Style::default().fg(Color::Yellow)),
        ]);
        frame.render_widget(Paragraph::new(line), rows[2]);
        return;
    }

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
    // The hint line follows focus: in the editor, surface the editing shortcuts;
    // elsewhere, the global navigation ones.
    let hint = if app.focus() == Focus::Editor {
        "  Ctrl-S save · Ctrl-F find · Ctrl-Z/Y undo/redo · Ctrl-X/C/V cut/copy/paste · Shift+↦ select · Tab focus"
    } else {
        "  Enter run · Ctrl-P palette · Ctrl-B explorer · PgUp/PgDn scroll · Tab focus · Ctrl-Q quit"
    };
    let line = Line::from(vec![
        Span::styled(
            " dadhichi ",
            Style::default().bg(Color::Green).fg(Color::Black),
        ),
        Span::raw(format!(" {} ", app.status)),
        Span::styled(format!("[{focus}]"), Style::default().fg(Color::DarkGray)),
        Span::raw(hint),
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

/// Build the styled spans for one editor line, overlaying the selection (if it
/// intersects this line) on top of syntax highlighting. `selection` is the
/// buffer-wide `(start, end)` char range; `line_start` is this line's first char
/// offset. The parts outside the selection are syntax-highlighted; the selected
/// part is drawn reversed. A line the selection misses is highlighted whole.
fn line_content_spans(
    text: &str,
    selection: Option<(usize, usize)>,
    line_start: usize,
    on_cursor: bool,
) -> Vec<Span<'static>> {
    let line_len = text.chars().count();
    if let Some((gs, ge)) = selection {
        // Clamp the selection to this line's visible columns [0, line_len].
        let start = gs.max(line_start).saturating_sub(line_start).min(line_len);
        let end = ge.min(line_start + line_len).saturating_sub(line_start);
        if end > start {
            let chars: Vec<char> = text.chars().collect();
            let before: String = chars[..start].iter().collect();
            let selected: String = chars[start..end].iter().collect();
            let after: String = chars[end..].iter().collect();
            let mut out = highlight_code(&before, on_cursor);
            out.push(Span::styled(
                selected,
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            out.extend(highlight_code(&after, on_cursor));
            return out;
        }
    }
    highlight_code(text, on_cursor)
}

/// A small, language-agnostic syntax highlighter for one line of code. It has no
/// dependency on a grammar: it colours line comments, string/char literals,
/// numbers, and a common set of keywords by scanning characters. That's enough to
/// give the editor legible colour across Rust/TS/Python/etc. without a heavyweight
/// parser. `bold` bolds every span (used for the cursor line).
fn highlight_code(text: &str, bold: bool) -> Vec<Span<'static>> {
    // A shared keyword set across curly-brace and Python-ish languages. Matching a
    // superset is fine — a stray highlight is far better than none.
    const KEYWORDS: &[&str] = &[
        "fn", "let", "mut", "const", "static", "struct", "enum", "trait", "impl", "pub", "use",
        "mod", "match", "if", "else", "for", "while", "loop", "return", "break", "continue",
        "async", "await", "move", "ref", "where", "as", "dyn", "self", "Self", "super", "crate",
        "type", "def", "class", "import", "from", "func", "var", "function", "public", "private",
        "protected", "new", "null", "true", "false", "None", "True", "False", "and", "or", "not",
        "in", "is", "with", "try", "catch", "except", "finally", "throw", "raise", "yield",
    ];
    let base = if bold { Modifier::BOLD } else { Modifier::empty() };
    let mk = |s: &str, colour: Color| {
        Span::styled(s.to_string(), Style::default().fg(colour).add_modifier(base))
    };

    // A whole-line comment (covers // and #). Cheap and common.
    let lead = text.trim_start();
    if lead.starts_with("//") || lead.starts_with('#') {
        return vec![mk(text, Color::DarkGray)];
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut word = String::new();

    // Flush an accumulated identifier/number as a styled span.
    fn flush(word: &mut String, spans: &mut Vec<Span<'static>>, base: Modifier, keywords: &[&str]) {
        if word.is_empty() {
            return;
        }
        let colour = if keywords.contains(&word.as_str()) {
            Color::Magenta
        } else if word.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '_')
            && word.chars().any(|c| c.is_ascii_digit())
        {
            Color::Yellow
        } else {
            Color::Reset
        };
        spans.push(Span::styled(
            std::mem::take(word),
            Style::default().fg(colour).add_modifier(base),
        ));
    }

    while i < chars.len() {
        let c = chars[i];
        // A string or char literal runs to its closing quote (respecting escapes).
        if c == '"' || c == '\'' || c == '`' {
            flush(&mut word, &mut spans, base, KEYWORDS);
            let quote = c;
            let mut lit = String::from(c);
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                lit.push(d);
                i += 1;
                if d == '\\' && i < chars.len() {
                    lit.push(chars[i]);
                    i += 1;
                    continue;
                }
                if d == quote {
                    break;
                }
            }
            spans.push(Span::styled(
                lit,
                Style::default().fg(Color::Green).add_modifier(base),
            ));
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '.' && word.chars().next().is_some_and(|w| w.is_ascii_digit()) {
            word.push(c);
        } else {
            flush(&mut word, &mut spans, base, KEYWORDS);
            spans.push(Span::styled(
                c.to_string(),
                Style::default().add_modifier(base),
            ));
        }
        i += 1;
    }
    flush(&mut word, &mut spans, base, KEYWORDS);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), Style::default().add_modifier(base)));
    }
    spans
}

/// Style one agent-console transcript line by its kind, so the model's answers
/// read clearly while tool activity and telemetry recede. The prefixes match the
/// markers `dadhichi-ui` writes into the chat buffer (`❯`, `↳`, `✓`, `✗`, `‹…›`,
/// `[event]`, `$`, `⇥`).
fn style_chat_line(line: &str) -> Line<'_> {
    let trimmed = line.trim_start();
    let (fg, modifier) = if trimmed.starts_with('❯') || trimmed.starts_with('$') {
        // The user's own goal / shell command echo — bright and bold.
        (Color::White, Modifier::BOLD)
    } else if trimmed.starts_with('↳') {
        // A tool call the agent is making.
        (Color::Cyan, Modifier::empty())
    } else if trimmed.starts_with('✓') {
        (Color::Green, Modifier::empty())
    } else if trimmed.starts_with('✗') {
        (Color::Red, Modifier::BOLD)
    } else if trimmed.starts_with('‹') {
        // The model-reply header line (‹assistant›).
        (Color::Yellow, Modifier::BOLD)
    } else if trimmed.starts_with('⇥') {
        // A delegation announcement.
        (Color::Magenta, Modifier::BOLD)
    } else if trimmed.starts_with('[') {
        // `[event.name] key=value` telemetry — dim so it doesn't shout.
        (Color::DarkGray, Modifier::empty())
    } else {
        // Model-reply body and everything else — the default readable foreground.
        (Color::Reset, Modifier::empty())
    };
    Line::from(Span::styled(
        line.to_string(),
        Style::default().fg(fg).add_modifier(modifier),
    ))
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

    /// Whether any cell in the rendered buffer uses `fg` as its foreground colour
    /// — used to assert that colouring (syntax, selection, phase) actually landed.
    fn buffer_uses_fg(terminal: &Terminal<TestBackend>, fg: Color) -> bool {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.fg == fg)
    }

    /// Whether any cell uses `bg` as its background colour.
    fn buffer_uses_bg(terminal: &Terminal<TestBackend>, bg: Color) -> bool {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.bg == bg)
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
    fn renders_delegation_review_panel() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.apply_event(&dadhichi_core::Event::new(
            "agent.delegation.review",
            serde_json::json!({
                "subagent": "code-agent",
                "verdict": "Completed · critic confidence 40% vs threshold 75%",
                "files": [{ "path": "src/added.rs", "deleted": false }]
            }),
        ));
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Delegation Review"), "review modal drawn");
        assert!(text.contains("src/added.rs"), "staged file listed");
        assert!(text.contains("y = commit"), "land prompt shown");
    }

    #[test]
    fn renders_inline_diagnostic_marker_in_the_editor() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        // demo_app opens src/main.rs with a warning on line 1 (0-based).
        let mut app = demo_app();
        app.set_focus(Focus::Editor);
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains('▲'),
            "warning marker drawn in the editor gutter"
        );
    }

    #[test]
    fn renders_all_panels() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Explorer"), "explorer panel drawn");
        assert!(text.contains("Editor"), "editor panel drawn");
        assert!(text.contains("Problems (1)"), "problems count shown");
        assert!(text.contains("Agent Console"), "chat panel drawn");
        assert!(text.contains("fn main()"), "editor content shown");
        assert!(text.contains("dadhichi"), "status bar drawn");
    }

    #[test]
    fn renders_the_editor_selection_reversed() {
        use ratatui::style::Modifier;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.set_focus(Focus::Editor);
        // Select the first few characters of the buffer ("fn m…").
        {
            let doc = app.active_document_mut().unwrap();
            for _ in 0..4 {
                doc.select_right();
            }
        }
        terminal.draw(|f| render(&mut app, f)).unwrap();

        // The selected span is drawn with the REVERSED modifier; no unselected
        // frame has it, so finding it proves the selection highlight landed.
        let reversed = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.modifier.contains(Modifier::REVERSED));
        assert!(reversed, "selection drawn with a reversed highlight");
    }

    #[test]
    fn hiding_the_explorer_reclaims_its_width() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.toggle_explorer(); // hide it
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(!text.contains("Explorer"), "explorer pane is gone");
        // The other panels are still drawn in the reclaimed space.
        assert!(text.contains("Editor"), "editor still drawn");
        assert!(text.contains("Agent Console"), "console still drawn");
    }

    #[test]
    fn scrolls_the_console_transcript_and_draws_a_scrollbar() {
        // A tall-enough terminal with far more transcript lines than fit forces
        // overflow. Hide the explorer so the console has ample width.
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mut app = demo_app();
        app.toggle_explorer();
        for i in 0..60 {
            app.push_chat(format!("row{i:02}"));
        }
        // Pinned to the tail: the newest line is visible, the oldest is not.
        terminal.draw(|f| render(&mut app, f)).unwrap();
        let tail = buffer_text(&terminal);
        assert!(tail.contains("row59"), "newest line visible at the tail");
        assert!(!tail.contains("row00"), "oldest line scrolled off at the tail");

        // Scroll all the way back into history: the oldest line comes into view
        // and the scrollbar thumb is drawn.
        app.chat_scroll_up(60);
        terminal.draw(|f| render(&mut app, f)).unwrap();
        let scrolled = buffer_text(&terminal);
        assert!(scrolled.contains('█'), "scrollbar thumb drawn on overflow");
        assert!(
            scrolled.contains("row00"),
            "oldest history is visible after scrolling to the top"
        );
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
        terminal.draw(|f| render(&mut app, f)).unwrap();

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
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("❯ fix"), "typed goal shown in the input line");
    }

    #[test]
    fn renders_goal_hint_when_empty_and_unfocused() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.set_focus(Focus::Editor); // move focus off the console
        terminal.draw(|f| render(&mut app, f)).unwrap();

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
        terminal.draw(|f| render(&mut app, f)).unwrap();

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
        terminal.draw(|f| render(&mut app, f)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Skills"), "skill-mode title drawn");
        assert!(text.contains("code-review"), "skill listed");
        assert!(text.contains("read_workspace"), "capability detail shown");
    }

    #[test]
    fn editor_syntax_highlights_keywords_strings_and_numbers() {
        // `let x = "hi" + 42;` exercises a keyword, a string literal, and a number.
        let mut spans = highlight_code(r#"let x = "hi" + 42;"#, false);
        // Collect (text, fg) pairs to assert each token got its colour.
        let colours: Vec<(String, Color)> = spans
            .drain(..)
            .map(|s| (s.content.into_owned(), s.style.fg.unwrap_or(Color::Reset)))
            .collect();
        assert!(
            colours.iter().any(|(t, c)| t == "let" && *c == Color::Magenta),
            "keyword coloured: {colours:?}"
        );
        assert!(
            colours.iter().any(|(t, c)| t == "\"hi\"" && *c == Color::Green),
            "string literal coloured: {colours:?}"
        );
        assert!(
            colours.iter().any(|(t, c)| t == "42" && *c == Color::Yellow),
            "number coloured: {colours:?}"
        );
    }

    #[test]
    fn editor_pane_actually_renders_colour() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app(); // opens src/main.rs containing `fn main()`
        app.set_focus(Focus::Editor);
        terminal.draw(|f| render(&mut app, f)).unwrap();
        // `fn` is a keyword → magenta must appear somewhere in the buffer.
        assert!(
            buffer_uses_fg(&terminal, Color::Magenta),
            "editor renders syntax colour for keywords"
        );
    }

    #[test]
    fn explorer_selection_is_highlighted() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.open_workspace("/proj");
        // Rebuild a deterministic tree and focus the explorer.
        app.explorer = Some(dadhichi_ui::Explorer::from_paths(
            "/proj",
            &["src/main.rs", "README.md"],
        ));
        app.set_focus(Focus::Explorer);
        terminal.draw(|f| render(&mut app, f)).unwrap();
        // The focused selection paints a green highlight background.
        assert!(
            buffer_uses_bg(&terminal, Color::Green),
            "selected explorer row is highlighted"
        );
        // The highlight symbol marks the row.
        assert!(
            buffer_text(&terminal).contains('▏'),
            "selection marker drawn"
        );
    }

    #[test]
    fn console_shows_an_animated_indicator_while_the_agent_works() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = demo_app();
        app.set_agent_running(true); // enters Thinking
        terminal.draw(|f| render(&mut app, f)).unwrap();
        let text = buffer_text(&terminal);
        // The phase label and a braille spinner glyph appear in the console title.
        assert!(text.contains("thinking"), "phase label shown: {text:?}");
        assert!(
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().any(|g| text.contains(g)),
            "a spinner glyph is drawn"
        );
    }

    #[test]
    fn console_transcript_is_colour_coded_by_line_kind() {
        // The model-reply header is yellow+bold.
        let header = style_chat_line("‹assistant›");
        assert_eq!(header.spans[0].style.fg, Some(Color::Yellow));
        // A tool call is cyan.
        let tool = style_chat_line("↳ fs.read({...})");
        assert_eq!(tool.spans[0].style.fg, Some(Color::Cyan));
        // Telemetry is dimmed.
        let telem = style_chat_line("[agent.status] status=running");
        assert_eq!(telem.spans[0].style.fg, Some(Color::DarkGray));
    }
}
