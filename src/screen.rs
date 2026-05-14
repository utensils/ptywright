use serde::{Deserialize, Serialize};

use crate::redaction::RedactionPolicy;
use crate::target::TerminalSize;

/// Cursor position and visibility from the rendered terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
}

/// Rendered style metadata for a terminal cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCellStyle {
    /// Foreground color as a stable debug string (`default`, `idx:N`, or `rgb:R:G:B`).
    pub foreground: String,
    /// Background color as a stable debug string (`default`, `idx:N`, or `rgb:R:G:B`).
    pub background: String,
    /// Bold style flag.
    pub bold: bool,
    /// Dim style flag.
    pub dim: bool,
    /// Italic style flag.
    pub italic: bool,
    /// Underline style flag.
    pub underline: bool,
    /// Inverse style flag.
    pub inverse: bool,
}

/// A rendered terminal cell in a screen snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCell {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
    /// Text content in the cell. Empty cells use an empty string.
    pub text: String,
    /// Whether this cell contains a wide character.
    pub wide: bool,
    /// Whether this cell is the continuation half of a wide character.
    pub wide_continuation: bool,
    /// Cell style metadata.
    pub style: ScreenCellStyle,
}

/// A rendered terminal snapshot suitable for automation decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// Terminal dimensions.
    pub size: TerminalSize,
    /// Cursor state.
    pub cursor: CursorState,
    /// Monotonic sequence number assigned by the session.
    pub sequence: u64,
    /// Visible text with terminal control sequences removed.
    pub plain_text: String,
    /// Rendered cell metadata in row-major order.
    pub cells: Vec<ScreenCell>,
    /// Whether the terminal is currently using the alternate screen.
    pub alternate_screen: bool,
    /// Whether application cursor mode is active.
    pub application_cursor: bool,
    /// Whether application keypad mode is active.
    pub application_keypad: bool,
    /// Window title when the parser/backend exposes it.
    pub title: Option<String>,
}

impl ScreenSnapshot {
    /// Return a copy with sensitive-looking text fields redacted by policy.
    #[must_use]
    pub fn redacted(&self, policy: &RedactionPolicy) -> Self {
        let mut snapshot = self.clone();
        snapshot.plain_text = policy.redact(&snapshot.plain_text);
        snapshot.title = snapshot.title.map(|title| policy.redact(&title));
        for cell in &mut snapshot.cells {
            cell.text = policy.redact(&cell.text);
        }
        snapshot
    }
}

trait TerminalEngine: Send {
    fn process(&mut self, bytes: &[u8]);
    fn resize(&mut self, size: TerminalSize);
    fn snapshot(&self, sequence: u64) -> ScreenSnapshot;
}

#[derive(Debug, Default)]
struct TerminalCallbacks {
    title: Option<String>,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }
}

struct Vt100TerminalEngine {
    parser: vt100::Parser<TerminalCallbacks>,
    size: TerminalSize,
}

impl Vt100TerminalEngine {
    fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                size.rows,
                size.cols,
                1_000,
                TerminalCallbacks::default(),
            ),
            size,
        }
    }
}

impl TerminalEngine for Vt100TerminalEngine {
    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn resize(&mut self, size: TerminalSize) {
        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.size = size;
    }

    fn snapshot(&self, sequence: u64) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let cells = (0..self.size.rows)
            .flat_map(|row| {
                (0..self.size.cols).filter_map(move |col| {
                    screen.cell(row, col).map(|cell| ScreenCell {
                        row,
                        col,
                        text: cell.contents().to_string(),
                        wide: cell.is_wide(),
                        wide_continuation: cell.is_wide_continuation(),
                        style: ScreenCellStyle {
                            foreground: color_name(cell.fgcolor()),
                            background: color_name(cell.bgcolor()),
                            bold: cell.bold(),
                            dim: cell.dim(),
                            italic: cell.italic(),
                            underline: cell.underline(),
                            inverse: cell.inverse(),
                        },
                    })
                })
            })
            .collect();

        ScreenSnapshot {
            size: self.size,
            cursor: CursorState {
                row: screen.cursor_position().0,
                col: screen.cursor_position().1,
                visible: !screen.hide_cursor(),
            },
            sequence,
            plain_text: screen.contents(),
            cells,
            alternate_screen: screen.alternate_screen(),
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
            title: self.parser.callbacks().title.clone(),
        }
    }
}

/// Incremental terminal parser and rendered screen state.
pub struct Terminal {
    engine: Box<dyn TerminalEngine>,
}

impl Terminal {
    /// Create a terminal parser for the given size.
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        Self {
            engine: Box::new(Vt100TerminalEngine::new(size)),
        }
    }

    /// Process bytes read from the PTY.
    pub fn process(&mut self, bytes: &[u8]) {
        self.engine.process(bytes);
    }

    /// Resize the terminal parser.
    pub fn resize(&mut self, size: TerminalSize) {
        self.engine.resize(size);
    }

    /// Return a snapshot for the provided session sequence number.
    #[must_use]
    pub fn snapshot(&self, sequence: u64) -> ScreenSnapshot {
        self.engine.snapshot(sequence)
    }
}

fn color_name(color: vt100::Color) -> String {
    match color {
        vt100::Color::Default => "default".to_string(),
        vt100::Color::Idx(index) => format!("idx:{index}"),
        vt100::Color::Rgb(red, green, blue) => format!("rgb:{red}:{green}:{blue}"),
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new(TerminalSize::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_snapshot_strips_ansi_sequences() {
        let mut terminal = Terminal::new(TerminalSize::new(4, 20));

        terminal.process(b"hello \x1b[31mred\x1b[0m");

        let snapshot = terminal.snapshot(7);
        assert_eq!(snapshot.sequence, 7);
        assert!(snapshot.plain_text.contains("hello red"));
        assert!(!snapshot.plain_text.contains("\x1b[31m"));
    }

    #[test]
    fn terminal_resize_updates_snapshot_size() {
        let mut terminal = Terminal::new(TerminalSize::new(4, 20));

        terminal.resize(TerminalSize::new(10, 40));

        assert_eq!(terminal.snapshot(0).size, TerminalSize::new(10, 40));
    }

    #[test]
    fn terminal_snapshot_includes_cells_and_modes() {
        let mut terminal = Terminal::new(TerminalSize::new(2, 4));

        terminal.process(b"A\x1b[31mB\x1b[0m");

        let snapshot = terminal.snapshot(1);
        assert_eq!(snapshot.cells.len(), 8);
        assert_eq!(snapshot.cells[0].text, "A");
        assert_eq!(snapshot.cells[1].text, "B");
        assert_eq!(snapshot.cells[1].style.foreground, "idx:1");
        assert!(!snapshot.alternate_screen);
    }

    #[test]
    fn terminal_snapshot_tracks_window_title() {
        let mut terminal = Terminal::new(TerminalSize::new(2, 10));

        terminal.process(b"\x1b]2;ptywright test\x07");

        assert_eq!(
            terminal.snapshot(1).title.as_deref(),
            Some("ptywright test")
        );
    }

    #[test]
    fn screen_snapshot_redacts_text_and_title() {
        let mut terminal = Terminal::new(TerminalSize::new(2, 40));

        terminal.process(b"\x1b]2;token=title-secret\x07password=hunter2");

        let snapshot = terminal.snapshot(1).redacted(&RedactionPolicy::default());
        assert!(!snapshot.plain_text.contains("hunter2"));
        assert_eq!(snapshot.title.as_deref(), Some("token=[REDACTED]"));
    }
}
