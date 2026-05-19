//! Frame-by-frame drawing for the ratatui REPL.
//!
//! Pure rendering: each function takes a [`Frame`] and the current
//! [`App`] state and writes widgets. No state mutation, no I/O, no
//! threads — this module is safe to call from the main loop's
//! `terminal.draw(...)` closure.
//!
//! Layout (top to bottom, vertical):
//!
//! ```text
//!   Tab strip          (height 1)
//!   Snapshot pane      (≥ 8 rows, ~55% of remaining space)
//!   Log scrollback     (remaining)
//!   Input box          (height 3, including border)
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::{App, LogEntry};
use super::highlighter;
use crate::screen::{ScreenCell, ScreenCellStyle, ScreenSnapshot};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let [tabs_area, snapshot_area, log_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(snapshot_height(area.height)),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(area);

    render_tab_strip(frame, tabs_area, app);
    render_snapshot(frame, snapshot_area, app);
    render_log(frame, log_area, app);
    render_input(frame, input_area, app);

    // If a completion popup is open, draw it as an overlay just above
    // the input box. It floats — drawn last so it occludes the log.
    if let Some(state) = &app.completions
        && !state.suggestions.is_empty()
    {
        render_completion_popup(frame, log_area, input_area, app);
    }
}

fn snapshot_height(total: u16) -> u16 {
    // Reserve 3 for input + 1 for tabs + 3 minimum for log. Of the
    // remainder, allocate 55% to the snapshot pane with a floor of 8
    // rows so the live screen is always at least minimally visible.
    let reserved = 1 + 3 + 3; // tabs + input + minimum log
    let remaining = total.saturating_sub(reserved);
    let snapshot = (remaining as u32 * 55 / 100) as u16;
    snapshot.max(8).min(total.saturating_sub(7))
}

fn render_tab_strip(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let ctx = app.ctx();
    let ctx = ctx.lock().expect("ctx mutex");
    let focused = ctx.focus.clone();
    let mut spans: Vec<Span<'static>> = Vec::new();
    if ctx.adapters.is_empty() {
        spans.push(Span::styled(
            " ptywright · no live sessions — try ",
            Style::default().add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(
            "session.spawn(\"claude-code\")",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            " ptywright ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        for (idx, tab) in ctx.adapters.iter().enumerate() {
            let is_focus = focused.as_deref() == Some(&tab.id);
            let body = match &tab.state_label {
                Some(label) => format!(" {} · {} · {} ", tab.id, tab.plugin, label),
                None => format!(" {} · {} ", tab.id, tab.plugin),
            };
            let style = if is_focus {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(body, style));
            if idx + 1 < ctx.adapters.len() {
                spans.push(Span::styled(
                    " │ ",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }
    }
    spans.push(Span::raw(" "));
    // Right-side label.
    let right = format!(" {} ", app.transport_label());
    let total_left: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_len = right.chars().count();
    let pad = (area.width as usize).saturating_sub(total_left + right_len);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(
        right,
        Style::default().add_modifier(Modifier::DIM),
    ));
    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, area);
}

fn render_snapshot(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().add_modifier(Modifier::DIM))
        .title(snapshot_title(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some((adapter, snap)) = app.focused_snapshot() else {
        let hint = if app
            .ctx()
            .lock()
            .map(|c| c.adapters.is_empty())
            .unwrap_or(true)
        {
            "session.spawn(\"claude-code\")  · spawn an adapter to start a session"
        } else {
            ":focus <adapter>  · pick an adapter to render in this pane"
        };
        let paragraph = Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
        ]))
        .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, inner);
        return;
    };
    let _ = adapter;
    let lines = snapshot_to_lines(snap, inner.width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn snapshot_title(app: &App) -> Line<'static> {
    if let Some((adapter, snap)) = app.focused_snapshot() {
        Line::from(vec![
            Span::styled(" snapshot ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                format!("· {} · ", adapter),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}×{}", snap.size.cols, snap.size.rows),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!(" · seq {} ", snap.sequence),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    } else {
        Line::from(Span::styled(
            " snapshot · (no focus) ",
            Style::default().add_modifier(Modifier::DIM),
        ))
    }
}

fn snapshot_to_lines(snap: &ScreenSnapshot, max_cols: u16) -> Vec<Line<'static>> {
    let rows = snap.size.rows as usize;
    let cols = snap.size.cols as usize;
    let mut by_row: Vec<Vec<&ScreenCell>> = (0..rows).map(|_| Vec::new()).collect();
    for cell in &snap.cells {
        let r = cell.row as usize;
        if r < rows {
            by_row[r].push(cell);
        }
    }
    let mut out = Vec::with_capacity(rows);
    for row in by_row {
        let mut cells = row;
        cells.sort_by_key(|c| c.col);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cells.len());
        let mut col = 0u16;
        for cell in cells {
            if cell.wide_continuation {
                continue;
            }
            if cell.col > col {
                spans.push(Span::raw(" ".repeat((cell.col - col) as usize)));
            }
            let text = if cell.text.is_empty() {
                " ".to_string()
            } else {
                cell.text.clone()
            };
            let width = cell.text.chars().count().max(1) as u16;
            col = cell.col + width;
            spans.push(Span::styled(text, cell_style(&cell.style)));
        }
        if (col as usize) < cols && (col < max_cols) {
            spans.push(Span::raw(" ".repeat(
                (cols.saturating_sub(col as usize)).min((max_cols - col) as usize),
            )));
        }
        out.push(Line::from(spans));
    }
    out
}

fn cell_style(style: &ScreenCellStyle) -> Style {
    let mut out = Style::default();
    if let Some(color) = parse_color(&style.foreground) {
        out = out.fg(color);
    }
    if let Some(color) = parse_color(&style.background) {
        out = out.bg(color);
    }
    let mut mods = Modifier::empty();
    if style.bold {
        mods |= Modifier::BOLD;
    }
    if style.dim {
        mods |= Modifier::DIM;
    }
    if style.italic {
        mods |= Modifier::ITALIC;
    }
    if style.underline {
        mods |= Modifier::UNDERLINED;
    }
    if style.inverse {
        mods |= Modifier::REVERSED;
    }
    if !mods.is_empty() {
        out = out.add_modifier(mods);
    }
    out
}

fn parse_color(value: &str) -> Option<Color> {
    if value == "default" {
        return None;
    }
    if let Some(idx) = value.strip_prefix("idx:")
        && let Ok(n) = idx.parse::<u8>()
    {
        return Some(Color::Indexed(n));
    }
    if let Some(rgb) = value.strip_prefix("rgb:") {
        let parts: Vec<&str> = rgb.split(':').collect();
        if parts.len() == 3 {
            let r = parts[0].parse().ok()?;
            let g = parts[1].parse().ok()?;
            let b = parts[2].parse().ok()?;
            return Some(Color::Rgb(r, g, b));
        }
    }
    None
}

fn render_log(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(app.log.len() * 2);
    for entry in &app.log {
        match entry {
            LogEntry::Input(text) => {
                let mut spans = vec![Span::styled(
                    "pty> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(highlighter::highlight(text, 0).into_iter().map(|s| Span {
                    content: s.content.into_owned().into(),
                    style: s.style,
                }));
                lines.push(Line::from(spans));
            }
            LogEntry::Note(note) => lines.push(note.to_line()),
            LogEntry::Line(text) => lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("↳", Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(text.clone(), Style::default().add_modifier(Modifier::DIM)),
            ])),
            LogEntry::Json(text) => lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("↳", Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(text.clone(), Style::default().add_modifier(Modifier::DIM)),
            ])),
            LogEntry::Error(text) => lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "✗",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(text.clone(), Style::default().fg(Color::Red)),
            ])),
            LogEntry::Notice(text) => lines.push(Line::from(vec![
                Span::styled("[notif] ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled(text.clone(), Style::default().add_modifier(Modifier::DIM)),
            ])),
            LogEntry::Help(text) => {
                for line in text.lines() {
                    let style = if line.ends_with(':') {
                        Style::default().fg(Color::Yellow)
                    } else if line.is_empty() {
                        Style::default()
                    } else {
                        Style::default().add_modifier(Modifier::DIM)
                    };
                    lines.push(Line::styled(line.to_string(), style));
                }
            }
            LogEntry::Banner(text) => lines.push(Line::styled(
                text.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            LogEntry::Hint(text) => lines.push(Line::styled(
                text.clone(),
                Style::default().fg(Color::Yellow),
            )),
        }
    }
    // Auto-scroll: pin the view to the bottom so newest entries are
    // always visible.
    let visible = area.height as usize;
    let scroll = lines.len().saturating_sub(visible) as u16;
    let paragraph = Paragraph::new(Text::from(lines))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().add_modifier(Modifier::DIM))
        .title(Line::from(vec![Span::styled(
            " pty> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let value = app.input.value();
    let cursor = app.input.cursor();
    // Apply syntax highlighting to the input buffer.
    let highlighted = highlighter::highlight(value, cursor);
    let spans: Vec<Span<'static>> = highlighted
        .into_iter()
        .map(|s| Span {
            content: s.content.into_owned().into(),
            style: s.style,
        })
        .collect();
    let line: Line<'static> = if spans.is_empty() {
        Line::from(Span::styled(
            "type :help or session.spawn(\"claude-code\")",
            Style::default().add_modifier(Modifier::DIM),
        ))
    } else {
        Line::from(spans)
    };
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, inner);

    // Place the cursor — ratatui needs an explicit set_cursor call when
    // we render text manually (we're not using a built-in input widget).
    if !value.is_empty() || app.completions.is_none() {
        let visual = app.input.visual_cursor() as u16;
        let x = inner.x + visual.min(inner.width.saturating_sub(1));
        frame.set_cursor_position((x, inner.y));
    }
}

fn render_completion_popup(frame: &mut Frame<'_>, _log_area: Rect, input_area: Rect, app: &App) {
    let Some(state) = &app.completions else {
        return;
    };
    let max_rows = 8u16;
    let rows = (state.suggestions.len() as u16).min(max_rows);
    let width = state
        .suggestions
        .iter()
        .map(|s| {
            s.value.chars().count()
                + s.description
                    .as_deref()
                    .map(|d| d.chars().count() + 3)
                    .unwrap_or(0)
        })
        .max()
        .unwrap_or(20)
        .min(input_area.width.saturating_sub(2) as usize) as u16
        + 4;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(rows + 1),
        width: width.min(input_area.width),
        height: rows + 2,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            " completions ",
            Style::default().fg(Color::Cyan),
        )));
    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);

    let start = state
        .index
        .saturating_sub((rows as usize).saturating_sub(1));
    let visible: Vec<&_> = state
        .suggestions
        .iter()
        .skip(start)
        .take(rows as usize)
        .collect();
    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, suggestion)| {
            let is_current = start + i == state.index;
            let style = if is_current {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(suggestion.value.clone(), style)];
            if let Some(desc) = &suggestion.description
                && !desc.is_empty()
            {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    desc.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines));
    frame.render_widget(paragraph, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_height_floors_at_minimum() {
        assert!(snapshot_height(40) >= 8);
        assert!(snapshot_height(20) >= 8);
        // Tiny terminal: snapshot height clamps to fit remaining space.
        let h = snapshot_height(15);
        assert!(h <= 15 - 7);
    }

    #[test]
    fn snapshot_height_scales_with_terminal() {
        let small = snapshot_height(30);
        let large = snapshot_height(80);
        assert!(large > small);
    }

    #[test]
    fn parse_color_handles_default_indexed_and_rgb() {
        assert_eq!(parse_color("default"), None);
        assert_eq!(parse_color("idx:9"), Some(Color::Indexed(9)));
        assert_eq!(parse_color("rgb:255:128:0"), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_color("garbage"), None);
    }
}
