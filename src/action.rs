use serde::{Deserialize, Serialize};

use crate::target::TerminalSize;

/// Named terminal keys encoded by ptywright.
///
/// The byte sequences in [`Key::bytes`] follow the conventional xterm /
/// VT100 encodings recognised by every modern terminal-mode application
/// (readline, vim, emacs, fzf, …). Function keys F1–F4 use the DEC SS3
/// form (`\x1bOP..S`); F5–F12 and the navigation cluster use the CSI
/// `\x1b[N~` form. `ShiftTab` is the standard back-tab `\x1b[Z`.
///
/// Variant naming follows `snake_case` on the wire because the enum
/// derives `serde(rename_all = "snake_case")`. Hyphens in caller-supplied
/// strings (e.g. `"shift-tab"`, `"ctrl-r"`) are normalised by plugins
/// before the value reaches this enum — see the `KEY_ALIASES` table in
/// `plugins/claude-code/main.lua` for the reference implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    // ── Submission / line editing ────────────────────────────────
    /// Enter / carriage return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Shift+Tab (back-tab, `CSI Z`).
    ShiftTab,
    /// Backspace.
    Backspace,
    /// Delete forward.
    Delete,
    /// Space — exposed as a named key for symmetry with the rest of the
    /// table; callers that want to type literal spaces should still
    /// prefer `action.text(" ")`.
    Space,

    // ── Arrows ───────────────────────────────────────────────────
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Right arrow.
    Right,
    /// Left arrow.
    Left,

    // ── Navigation cluster ───────────────────────────────────────
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Insert.
    Insert,

    // ── Ctrl combos ──────────────────────────────────────────────
    //
    // `CtrlH`/`CtrlI`/`CtrlJ`/`CtrlM` are intentionally omitted —
    // those bytes are already covered by `Backspace` / `Tab` /
    // (linefeed; rarely useful as a named key) / `Enter`. Use the
    // semantic name instead so transcripts stay readable.
    /// Ctrl-A — line start in readline / emacs.
    CtrlA,
    /// Ctrl-B — back one char.
    CtrlB,
    /// Ctrl-C interrupt byte.
    CtrlC,
    /// Ctrl-D EOF byte.
    CtrlD,
    /// Ctrl-E — line end in readline / emacs.
    CtrlE,
    /// Ctrl-F — forward one char.
    CtrlF,
    /// Ctrl-G — bell / cancel.
    CtrlG,
    /// Ctrl-K — kill to end of line.
    CtrlK,
    /// Ctrl-L — clear / refresh.
    CtrlL,
    /// Ctrl-N — next line / history forward.
    CtrlN,
    /// Ctrl-O — newline-and-yank in readline.
    CtrlO,
    /// Ctrl-P — previous line / history back.
    CtrlP,
    /// Ctrl-Q — XON / quoted-insert.
    CtrlQ,
    /// Ctrl-R — reverse search.
    CtrlR,
    /// Ctrl-S — XOFF / forward search.
    CtrlS,
    /// Ctrl-T — transpose chars.
    CtrlT,
    /// Ctrl-U — kill to line start.
    CtrlU,
    /// Ctrl-V — verbatim-insert.
    CtrlV,
    /// Ctrl-W — kill previous word.
    CtrlW,
    /// Ctrl-X — chord prefix in emacs / nano.
    CtrlX,
    /// Ctrl-Y — yank.
    CtrlY,
    /// Ctrl-Z — suspend.
    CtrlZ,

    // ── Function keys ────────────────────────────────────────────
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl Key {
    /// Encode a key as bytes written to the PTY master.
    #[must_use]
    pub const fn bytes(&self) -> &'static [u8] {
        match self {
            // Submission / line editing
            Self::Enter => b"\r",
            Self::Escape => b"\x1b",
            Self::Tab => b"\t",
            Self::ShiftTab => b"\x1b[Z",
            Self::Backspace => b"\x7f",
            Self::Delete => b"\x1b[3~",
            Self::Space => b" ",

            // Arrows
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",

            // Navigation cluster
            Self::Home => b"\x1b[H",
            Self::End => b"\x1b[F",
            Self::PageUp => b"\x1b[5~",
            Self::PageDown => b"\x1b[6~",
            Self::Insert => b"\x1b[2~",

            // Ctrl combos
            Self::CtrlA => b"\x01",
            Self::CtrlB => b"\x02",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlE => b"\x05",
            Self::CtrlF => b"\x06",
            Self::CtrlG => b"\x07",
            Self::CtrlK => b"\x0b",
            Self::CtrlL => b"\x0c",
            Self::CtrlN => b"\x0e",
            Self::CtrlO => b"\x0f",
            Self::CtrlP => b"\x10",
            Self::CtrlQ => b"\x11",
            Self::CtrlR => b"\x12",
            Self::CtrlS => b"\x13",
            Self::CtrlT => b"\x14",
            Self::CtrlU => b"\x15",
            Self::CtrlV => b"\x16",
            Self::CtrlW => b"\x17",
            Self::CtrlX => b"\x18",
            Self::CtrlY => b"\x19",
            Self::CtrlZ => b"\x1a",

            // Function keys
            Self::F1 => b"\x1bOP",
            Self::F2 => b"\x1bOQ",
            Self::F3 => b"\x1bOR",
            Self::F4 => b"\x1bOS",
            Self::F5 => b"\x1b[15~",
            Self::F6 => b"\x1b[17~",
            Self::F7 => b"\x1b[18~",
            Self::F8 => b"\x1b[19~",
            Self::F9 => b"\x1b[20~",
            Self::F10 => b"\x1b[21~",
            Self::F11 => b"\x1b[23~",
            Self::F12 => b"\x1b[24~",
        }
    }
}

/// Process signal sent to a session's child via [`Action::Signal`] or
/// [`crate::Session::signal`].
///
/// `Int` and `Kill` overlap with [`Action::Interrupt`] / [`Action::Kill`];
/// they're retained here so the signal surface is exhaustive and so callers
/// composing higher-level controllers (e.g. SIGTERM-then-SIGKILL ladders via
/// [`crate::Session::terminate`]) don't have to mix two enums.
///
/// Cross-platform behaviour:
///
/// * Unix: every variant maps directly to its `libc` constant.
/// * Windows: `Term` maps to a best-effort `taskkill /T /PID`, `Kill` is a
///   hard kill via the existing `ChildKiller` path, and `Int` writes the
///   Ctrl-C byte to the PTY (same as [`Action::Interrupt`]). The other
///   variants return [`crate::Error::UnsupportedOnPlatform`] — Windows has
///   no POSIX equivalent for SIGHUP / SIGQUIT / SIGUSR1 / SIGUSR2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// SIGTERM — graceful termination.
    Term,
    /// SIGHUP — terminal hangup.
    Hup,
    /// SIGQUIT — quit with core dump.
    Quit,
    /// SIGINT — interrupt (same byte as Ctrl-C via the PTY).
    Int,
    /// SIGKILL — uncatchable hard kill.
    Kill,
    /// SIGUSR1 — user-defined #1.
    User1,
    /// SIGUSR2 — user-defined #2.
    User2,
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
    /// Send an arbitrary process signal — see [`Signal`].
    Signal(Signal),
    /// Kill the child process.
    Kill,
    /// Record a label-keyed marker at the current transcript cursor. No
    /// PTY bytes are written. See [`crate::Transcript::mark`] for the
    /// storage semantics (capped at `MAX_MARKERS = 64` distinct labels;
    /// repeated calls with the same label overwrite the cursor). This
    /// variant lets plugins request transcript segmentation as part of
    /// their action plans — the host applies it via
    /// [`crate::Session::mark_transcript`] alongside any PTY-mutating
    /// actions in the same plan. Plugins that want to mark from a
    /// classifier (where there is no plan) declare a
    /// [`HostMark`](crate::extension::HostMark) on the returned
    /// snapshot instead.
    MarkTranscript { label: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_terminal_sequences() {
        // Original surface — unchanged.
        assert_eq!(Key::Enter.bytes(), b"\r");
        assert_eq!(Key::CtrlC.bytes(), b"\x03");
        assert_eq!(Key::Up.bytes(), b"\x1b[A");
    }

    #[test]
    fn shift_tab_encodes_back_tab_csi_z() {
        // The single most asked-for combo when this surface was
        // expanded — pin it explicitly so a future refactor of the
        // table doesn't silently regress to plain `\t`.
        assert_eq!(Key::ShiftTab.bytes(), b"\x1b[Z");
    }

    #[test]
    fn navigation_cluster_encodes_csi_tildes() {
        assert_eq!(Key::Home.bytes(), b"\x1b[H");
        assert_eq!(Key::End.bytes(), b"\x1b[F");
        assert_eq!(Key::PageUp.bytes(), b"\x1b[5~");
        assert_eq!(Key::PageDown.bytes(), b"\x1b[6~");
        assert_eq!(Key::Insert.bytes(), b"\x1b[2~");
        assert_eq!(Key::Delete.bytes(), b"\x1b[3~");
        assert_eq!(Key::Space.bytes(), b" ");
    }

    #[test]
    fn ctrl_combos_encode_control_bytes() {
        assert_eq!(Key::CtrlA.bytes(), b"\x01");
        assert_eq!(Key::CtrlR.bytes(), b"\x12");
        assert_eq!(Key::CtrlU.bytes(), b"\x15");
        assert_eq!(Key::CtrlW.bytes(), b"\x17");
        assert_eq!(Key::CtrlZ.bytes(), b"\x1a");
    }

    #[test]
    fn function_keys_encode_ss3_and_csi_forms() {
        // F1–F4 use DEC SS3; F5+ use CSI `N~` with the canonical
        // xterm gap between 16 and 17 (no F-key uses 16~).
        assert_eq!(Key::F1.bytes(), b"\x1bOP");
        assert_eq!(Key::F4.bytes(), b"\x1bOS");
        assert_eq!(Key::F5.bytes(), b"\x1b[15~");
        assert_eq!(Key::F12.bytes(), b"\x1b[24~");
    }

    #[test]
    fn key_deserialises_from_snake_case_strings() {
        // The wire shape we depend on: every variant round-trips
        // through serde as snake_case. This is what makes the plugin
        // pass `action.key("shift_tab")` directly without a custom
        // deserializer.
        assert_eq!(
            serde_json::from_str::<Key>(r#""shift_tab""#).unwrap(),
            Key::ShiftTab
        );
        assert_eq!(
            serde_json::from_str::<Key>(r#""page_up""#).unwrap(),
            Key::PageUp
        );
        assert_eq!(
            serde_json::from_str::<Key>(r#""ctrl_r""#).unwrap(),
            Key::CtrlR
        );
        assert_eq!(serde_json::from_str::<Key>(r#""f7""#).unwrap(), Key::F7);
    }
}
