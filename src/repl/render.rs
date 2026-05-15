//! Pure-function rendering helpers for the TUI.
//!
//! Everything here takes ptywright's own data shapes (`ScreenSnapshot`,
//! `ReplCtx`) and produces `ratatui::text::Text<'static>` or `Line` values
//! that the TUI layer can drop into widgets. No I/O, no terminal handles —
//! easy to unit-test against existing fixtures.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use super::ctx::{HistoryEntry, OutcomeKind, ReplCtx};
use crate::screen::{ScreenCell, ScreenCellStyle, ScreenSnapshot};

/// Convert a screen snapshot into a multi-line `Text<'static>` that
/// preserves cell-level foreground/background and bold/underline/inverse
/// flags. Wide-character continuation cells are skipped (the wide cell to
/// their left already carries the full glyph), matching how a terminal
/// emulator would render them.
pub fn render_snapshot(snapshot: &ScreenSnapshot) -> Text<'static> {
    let mut rows: Vec<Vec<&ScreenCell>> = (0..snapshot.size.rows as usize)
        .map(|_| Vec::new())
        .collect();
    for cell in &snapshot.cells {
        let row_idx = cell.row as usize;
        if row_idx < rows.len() {
            rows[row_idx].push(cell);
        }
    }
    for row in &mut rows {
        row.sort_by_key(|cell| cell.col);
    }

    let lines = rows
        .into_iter()
        .map(|row_cells| {
            let spans = collapse_run(row_cells);
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

/// Collapse contiguous cells with the same style into one `Span`. Empty
/// strings are emitted as a single space so trailing whitespace still
/// renders with the expected background color.
fn collapse_run(cells: Vec<&ScreenCell>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut current_style: Option<Style> = None;
    for cell in cells {
        if cell.wide_continuation {
            // The previous wide cell already wrote the glyph.
            continue;
        }
        let style = cell_style_to_ratatui(&cell.style);
        let text = if cell.text.is_empty() {
            " ".to_string()
        } else {
            cell.text.clone()
        };
        match current_style {
            Some(existing) if existing == style => {
                buffer.push_str(&text);
            }
            _ => {
                if let Some(style) = current_style.take()
                    && !buffer.is_empty()
                {
                    spans.push(Span::styled(std::mem::take(&mut buffer), style));
                }
                current_style = Some(style);
                buffer.push_str(&text);
            }
        }
    }
    if let Some(style) = current_style
        && !buffer.is_empty()
    {
        spans.push(Span::styled(buffer, style));
    }
    spans
}

/// Convert ptywright's stable-debug color/style strings into a ratatui
/// `Style`. Unknown color shapes (future palette extensions) fall back to
/// the terminal's default so rendering never panics.
pub fn cell_style_to_ratatui(style: &ScreenCellStyle) -> Style {
    let mut out = Style::default();
    if let Some(color) = parse_color(&style.foreground) {
        out = out.fg(color);
    }
    if let Some(color) = parse_color(&style.background) {
        out = out.bg(color);
    }
    let mut modifier = Modifier::empty();
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.dim {
        modifier |= Modifier::DIM;
    }
    if style.italic {
        modifier |= Modifier::ITALIC;
    }
    if style.underline {
        modifier |= Modifier::UNDERLINED;
    }
    if style.inverse {
        modifier |= Modifier::REVERSED;
    }
    if !modifier.is_empty() {
        out = out.add_modifier(modifier);
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

/// Render the tab strip as a single ratatui `Line`. The focused tab is
/// wrapped in brackets and bolded; unfocused tabs render plain.
pub fn render_tabs(ctx: &ReplCtx) -> Line<'static> {
    if ctx.adapters.is_empty() {
        return Line::from(Span::styled(
            "(no adapters · session.spawn(\"…\") to start one)".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (idx, tab) in ctx.adapters.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        let is_focused = ctx.focus.as_deref() == Some(tab.id.as_str());
        let state = tab.state_label.as_deref().unwrap_or("ready");
        let label = format!("{}: {} · {}", tab.id, tab.plugin, state);
        let style = if is_focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!("[{label}]"), style));
    }
    Line::from(spans)
}

/// Render the rolling history pane as a `Text<'static>`. Most recent entries
/// appear at the bottom.
pub fn render_history(ctx: &ReplCtx) -> Text<'static> {
    let lines = ctx
        .history
        .iter()
        .flat_map(|entry| history_entry_lines(entry))
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn history_entry_lines(entry: &HistoryEntry) -> Vec<Line<'static>> {
    let (glyph, glyph_style) = match entry.kind {
        OutcomeKind::Ok => (
            "✓",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        OutcomeKind::Json => (
            "↪",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        OutcomeKind::Note => ("·", Style::default().add_modifier(Modifier::DIM)),
        OutcomeKind::Error => (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };

    let mut lines = Vec::with_capacity(2);
    lines.push(Line::from(vec![
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::raw(entry.input.clone()),
    ]));
    if let Some(detail) = entry.detail.as_ref() {
        // Split on '\n' so multi-line outputs (e.g. `:help`) render as
        // multiple rows in the history pane instead of one overflowing
        // line.
        for chunk in detail.split('\n') {
            if chunk.is_empty() {
                lines.push(Line::from(""));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        chunk.to_string(),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CursorState, ScreenCell, ScreenCellStyle, ScreenSnapshot};
    use crate::target::TerminalSize;

    fn plain_cell(row: u16, col: u16, ch: &str) -> ScreenCell {
        ScreenCell {
            row,
            col,
            text: ch.to_string(),
            wide: false,
            wide_continuation: false,
            style: ScreenCellStyle {
                foreground: "default".into(),
                background: "default".into(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
            },
        }
    }

    fn snapshot_with(rows: u16, cols: u16, cells: Vec<ScreenCell>) -> ScreenSnapshot {
        ScreenSnapshot {
            size: TerminalSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            cursor: CursorState {
                row: 0,
                col: 0,
                visible: true,
            },
            sequence: 1,
            plain_text: String::new(),
            cells,
            alternate_screen: false,
            application_cursor: false,
            application_keypad: false,
            title: None,
        }
    }

    #[test]
    fn render_snapshot_groups_cells_by_row() {
        let snapshot = snapshot_with(
            2,
            3,
            vec![
                plain_cell(0, 0, "a"),
                plain_cell(0, 1, "b"),
                plain_cell(0, 2, "c"),
                plain_cell(1, 0, "d"),
                plain_cell(1, 1, "e"),
                plain_cell(1, 2, "f"),
            ],
        );
        let text = render_snapshot(&snapshot);
        assert_eq!(text.lines.len(), 2);
        let row0: String = text.lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let row1: String = text.lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(row0, "abc");
        assert_eq!(row1, "def");
    }

    #[test]
    fn render_snapshot_collapses_contiguous_style_runs() {
        let mut bold = plain_cell(0, 0, "x");
        bold.style.bold = true;
        let mut bold2 = plain_cell(0, 1, "y");
        bold2.style.bold = true;
        let plain = plain_cell(0, 2, "z");
        let snapshot = snapshot_with(1, 3, vec![bold, bold2, plain]);
        let text = render_snapshot(&snapshot);
        assert_eq!(text.lines[0].spans.len(), 2);
        assert_eq!(text.lines[0].spans[0].content.as_ref(), "xy");
        assert_eq!(text.lines[0].spans[1].content.as_ref(), "z");
    }

    #[test]
    fn render_snapshot_skips_wide_continuation_cells() {
        let mut wide = plain_cell(0, 0, "■");
        wide.wide = true;
        let mut cont = plain_cell(0, 1, "");
        cont.wide_continuation = true;
        let snapshot = snapshot_with(1, 2, vec![wide, cont]);
        let text = render_snapshot(&snapshot);
        let row: String = text.lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(row, "■");
    }

    #[test]
    fn parse_color_handles_idx_and_rgb_and_default() {
        assert_eq!(parse_color("default"), None);
        assert_eq!(parse_color("idx:9"), Some(Color::Indexed(9)));
        assert_eq!(parse_color("rgb:255:128:0"), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_color("rgb:not-numeric"), None);
    }

    #[test]
    fn render_tabs_marks_focused_with_bold() {
        let mut ctx = ReplCtx::new();
        ctx.upsert_adapter("e1", "claude-code");
        ctx.upsert_adapter("e2", "claude-code");
        ctx.focus = Some("e2".into());
        let line = render_tabs(&ctx);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("e1"));
        assert!(rendered.contains("e2"));
        let bold_focused = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .any(|s| s.content.contains("e2"));
        assert!(bold_focused, "focused tab should be bold: `{rendered}`");
    }

    #[test]
    fn render_tabs_falls_back_to_placeholder_when_empty() {
        let ctx = ReplCtx::new();
        let line = render_tabs(&ctx);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("session.spawn"),
            "expected hint, got `{rendered}`"
        );
    }

    #[test]
    fn render_history_emits_glyph_per_entry() {
        let mut ctx = ReplCtx::new();
        ctx.record(HistoryEntry {
            input: "plugins()".into(),
            kind: OutcomeKind::Json,
            detail: Some("3 plugins".into()),
        });
        ctx.record(HistoryEntry {
            input: ":quit".into(),
            kind: OutcomeKind::Ok,
            detail: None,
        });
        let text = render_history(&ctx);
        // Two entries: the first has detail (2 lines), the second has no
        // detail (1 line) → 3 lines total.
        assert_eq!(text.lines.len(), 3);
    }
}
