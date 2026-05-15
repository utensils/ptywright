use serde::{Deserialize, Serialize};

use crate::target::TerminalSize;

/// Named terminal keys encoded by ptywright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    /// Enter / carriage return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Right arrow.
    Right,
    /// Left arrow.
    Left,
    /// Ctrl-C interrupt byte.
    CtrlC,
    /// Ctrl-D EOF byte.
    CtrlD,
}

impl Key {
    /// Encode a key as bytes written to the PTY master.
    #[must_use]
    pub const fn bytes(&self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Escape => b"\x1b",
            Self::Tab => b"\t",
            Self::Backspace => b"\x7f",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
        }
    }
}

/// Input or lifecycle action sent to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Action {
    /// Write text bytes to the PTY.
    Text(String),
    /// Send a named key.
    Key(Key),
    /// Paste text as raw bytes — identical wire shape to [`Action::Text`],
    /// retained as a distinct variant so callers and plugins can express
    /// intent ("this came from a paste") even when the bytes go straight
    /// to the PTY. Use [`Action::BracketedPaste`] when the receiver has
    /// enabled bracketed paste mode and a real paste boundary matters.
    Paste(String),
    /// Paste text wrapped in bracketed-paste markers (`CSI 200 ~` …
    /// `CSI 201 ~`). Use this for receivers that have set DECSET 2004
    /// (Claude Code v2.1+, vim, fish, …) — the wrapper lets the
    /// receiver treat the payload as a single paste so a subsequent
    /// Enter key is interpreted as a submit rather than absorbed into
    /// the paste tokeniser. Do not use against receivers that have not
    /// enabled bracketed paste; the wrapper bytes would land in the
    /// child as literal characters.
    BracketedPaste(String),
    /// Resize the PTY and terminal parser.
    Resize(TerminalSize),
    /// Send Ctrl-C.
    Interrupt,
    /// Send Ctrl-D.
    Eof,
    /// Kill the child process.
    Kill,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_terminal_sequences() {
        assert_eq!(Key::Enter.bytes(), b"\r");
        assert_eq!(Key::CtrlC.bytes(), b"\x03");
        assert_eq!(Key::Up.bytes(), b"\x1b[A");
    }
}
