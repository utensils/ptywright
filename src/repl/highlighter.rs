//! reedline `Highlighter` impl for the REPL's DSL.
//!
//! Intentionally regex-free at runtime: a small character-driven scanner
//! recognises identifiers, dotted paths, string / regex / duration / int
//! literals, and the `:meta` prefix. Mapping each token kind to a
//! `nu_ansi_term::Style` keeps the dependency footprint small (no syntect)
//! while still producing a Rails-console-style coloured prompt.

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

/// Token categories recognised by the highlighter. Kept private — the
/// scanner emits these directly; the only public surface is `StyledText`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Whitespace,
    Method,
    Keyword,
    String,
    Regex,
    Duration,
    Int,
    Meta,
    Punctuation,
    Other,
}

const METHOD_NAMES: &[&str] = &[
    "plugins",
    "session",
    "spawn",
    "list",
    "close",
    "focus",
    "state",
    "send",
    "text",
    "key",
    "intent",
    "wait",
    "matches",
    "screen_stable",
    "transcript",
    "snapshot",
    "screen",
    "inspect",
];

const KEYWORDS: &[&str] = &["true", "false", "null", "nil"];

fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::Whitespace => Style::new(),
        Kind::Method => Color::Cyan.bold(),
        Kind::Keyword => Color::Purple.bold(),
        Kind::String => Color::Green.normal(),
        Kind::Regex => Color::Yellow.normal(),
        Kind::Duration => Color::LightYellow.normal(),
        Kind::Int => Color::LightCyan.normal(),
        Kind::Meta => Color::Magenta.bold(),
        Kind::Punctuation => Style::new().dimmed(),
        Kind::Other => Style::new(),
    }
}

/// reedline-facing highlighter — no state, no allocations beyond what
/// `StyledText` requires.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplHighlighter;

impl ReplHighlighter {
    pub fn new() -> Self {
        Self
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        for (kind, slice) in scan(line) {
            styled.push((style_for(kind), slice.to_string()));
        }
        styled
    }
}

fn scan(line: &str) -> Vec<(Kind, &str)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let ch = bytes[i] as char;

        // `:meta` prefix — the whole identifier after the colon is meta.
        if ch == ':' && i == start_of_logical_line(line, i) {
            i += 1;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            out.push((Kind::Meta, &line[start..i]));
            continue;
        }

        if ch.is_whitespace() {
            while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                i += 1;
            }
            out.push((Kind::Whitespace, &line[start..i]));
            continue;
        }

        if ch == '"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] as char == '\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] as char == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push((Kind::String, &line[start..i]));
            continue;
        }

        if ch == 'r' && bytes.get(i + 1).is_some_and(|b| *b as char == '"') {
            i += 2;
            while i < bytes.len() && bytes[i] as char != '"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            out.push((Kind::Regex, &line[start..i]));
            continue;
        }

        if ch.is_ascii_digit()
            || (ch == '-' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()))
        {
            if ch == '-' {
                i += 1;
            }
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            // Optional duration unit.
            let after_digits = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_alphabetic() {
                i += 1;
            }
            let kind = if i > after_digits {
                Kind::Duration
            } else {
                Kind::Int
            };
            out.push((kind, &line[start..i]));
            continue;
        }

        if is_ident_start(ch) {
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if KEYWORDS.contains(&word) {
                Kind::Keyword
            } else if METHOD_NAMES.contains(&word) {
                Kind::Method
            } else {
                Kind::Other
            };
            out.push((kind, word));
            continue;
        }

        // Single-character punctuation / fallback.
        i += 1;
        let kind = match ch {
            '.' | '(' | ')' | ',' | '=' => Kind::Punctuation,
            _ => Kind::Other,
        };
        out.push((kind, &line[start..i]));
    }
    out
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_byte(b: u8) -> bool {
    let ch = b as char;
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Index of the first non-whitespace byte at or before `i` in `line`. Used
/// to confine the `:meta` prefix rule to the start of the logical input —
/// inside `session.spawn(":foo")` the colon is part of a string, not a
/// meta marker.
fn start_of_logical_line(line: &str, i: usize) -> usize {
    let bytes = line.as_bytes();
    let mut start = 0;
    while start < i {
        let ch = bytes[start] as char;
        if ch.is_whitespace() {
            start += 1;
        } else {
            break;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(Kind, String)> {
        scan(line)
            .into_iter()
            .map(|(k, s)| (k, s.to_string()))
            .collect()
    }

    #[test]
    fn highlights_method_chain_and_string_arg() {
        let out = kinds(r#"session.spawn("claude-code")"#);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Method && s == "session")
        );
        assert!(out.iter().any(|(k, s)| *k == Kind::Method && s == "spawn"));
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::String && s == "\"claude-code\"")
        );
    }

    #[test]
    fn highlights_raw_regex_literal() {
        let out = kinds(r#"wait.matches(r"^Approve\? \(y/n\)")"#);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Regex && s.starts_with("r\""))
        );
    }

    #[test]
    fn highlights_duration_and_int() {
        let out = kinds("wait.screen_stable(250ms)");
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Duration && s == "250ms")
        );
        let out = kinds("session.spawn(\"x\", rows=24, cols=80)");
        assert!(out.iter().any(|(k, s)| *k == Kind::Int && s == "24"));
        assert!(out.iter().any(|(k, s)| *k == Kind::Int && s == "80"));
    }

    #[test]
    fn highlights_meta_prefix_only_at_start() {
        let out = kinds(":focus e2");
        assert!(out.iter().any(|(k, s)| *k == Kind::Meta && s == ":focus"));

        // A colon inside a string must not flip the meta highlight on.
        let inside = kinds(r#"send.text(":fake")"#);
        let inside_meta = inside.iter().any(|(k, _)| *k == Kind::Meta);
        assert!(!inside_meta, "string-internal colon must not be Meta");
    }

    #[test]
    fn highlights_keyword_true_false_null() {
        let out = kinds("transcript.snapshot(redact=false)");
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "false"));
    }

    #[test]
    fn negative_integer_keeps_int_kind() {
        // The lexer accepts a leading `-` as part of an integer; the
        // highlighter must mirror that or `count=-5` paints the digits
        // separately from the sign.
        let out = kinds("send.intent(\"x\", count=-5)");
        assert!(out.iter().any(|(k, s)| *k == Kind::Int && s == "-5"));
    }

    #[test]
    fn unterminated_string_does_not_panic_and_returns_string_kind() {
        // The user is mid-typing — the highlighter must paint the
        // partial token as a String span so the open quote is visible
        // rather than swallowed as Other.
        let out = kinds(r#"send.text("hello"#);
        assert!(out.iter().any(|(k, _)| *k == Kind::String));
    }

    #[test]
    fn unterminated_raw_regex_does_not_panic() {
        let out = kinds(r#"wait(matches(r"^x"#);
        assert!(out.iter().any(|(k, _)| *k == Kind::Regex));
    }

    #[test]
    fn punctuation_kind_assigned_to_dot_paren_comma_equals() {
        let out = kinds("session.spawn(\"x\", rows=24)");
        assert!(out.iter().any(|(k, s)| *k == Kind::Punctuation && s == "."));
        assert!(out.iter().any(|(k, s)| *k == Kind::Punctuation && s == "("));
        assert!(out.iter().any(|(k, s)| *k == Kind::Punctuation && s == ","));
        assert!(out.iter().any(|(k, s)| *k == Kind::Punctuation && s == "="));
        assert!(out.iter().any(|(k, s)| *k == Kind::Punctuation && s == ")"));
    }

    #[test]
    fn leading_whitespace_does_not_disable_meta_prefix() {
        // The TUI input strip pads the prompt with a leading space
        // sometimes; the highlighter should still treat `:foo` as Meta
        // because the first non-whitespace byte is a colon.
        let out = kinds("   :tabs");
        assert!(out.iter().any(|(k, s)| *k == Kind::Meta && s == ":tabs"));
    }

    #[test]
    fn highlighter_handle_returns_styled_text_in_kind_order() {
        // The reedline-facing `highlight` impl pushes one styled span
        // per scan token. Confirm the impl doesn't drop tokens.
        let h = ReplHighlighter::new();
        let styled = h.highlight("session.spawn(\"x\")", 0);
        // 7 tokens: `session`, `.`, `spawn`, `(`, `"x"`, `)`, …
        assert!(
            styled.buffer.len() >= 6,
            "expected ≥6 styled spans, got {}",
            styled.buffer.len()
        );
    }

    #[test]
    fn style_for_assigns_distinct_styles_per_kind() {
        // Style equality is a poor man's "the table is wired up" smoke
        // test — if a future contributor adds a Kind and forgets to
        // extend `style_for`, the default `Style::new()` would silently
        // un-style it.
        assert_ne!(style_for(Kind::Method), style_for(Kind::Other));
        assert_ne!(style_for(Kind::String), style_for(Kind::Regex));
        assert_ne!(style_for(Kind::Int), style_for(Kind::Duration));
        assert_ne!(style_for(Kind::Meta), style_for(Kind::Keyword));
    }
}
