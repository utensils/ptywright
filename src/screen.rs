use crate::target::TerminalSize;

/// Cursor position and visibility from the rendered terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
}

/// A rendered terminal snapshot suitable for automation decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    /// Terminal dimensions.
    pub size: TerminalSize,
    /// Cursor state.
    pub cursor: CursorState,
    /// Monotonic sequence number assigned by the session.
    pub sequence: u64,
    /// Visible text with terminal control sequences removed.
    pub plain_text: String,
}

/// Incremental terminal parser and rendered screen state.
pub struct Terminal {
    parser: vt100::Parser,
    size: TerminalSize,
}

impl Terminal {
    /// Create a terminal parser for the given size.
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, 1_000),
            size,
        }
    }

    /// Process bytes read from the PTY.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the terminal parser.
    pub fn resize(&mut self, size: TerminalSize) {
        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.size = size;
    }

    /// Return a snapshot for the provided session sequence number.
    #[must_use]
    pub fn snapshot(&self, sequence: u64) -> ScreenSnapshot {
        let screen = self.parser.screen();
        ScreenSnapshot {
            size: self.size,
            cursor: CursorState {
                row: screen.cursor_position().0,
                col: screen.cursor_position().1,
                visible: !screen.hide_cursor(),
            },
            sequence,
            plain_text: screen.contents(),
        }
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
}
