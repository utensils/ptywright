//! reedline `Highlighter` impl for Lua REPL input.
//!
//! Two changes from the legacy DSL highlighter:
//!
//! 1. Recognised identifiers are sourced from the **live REPL-globals
//!    whitelist** (`session`, `send`, `wait`, `matches`, …) rather than a
//!    hand-maintained `METHOD_NAMES` table. New bindings light up
//!    automatically once they're installed in [`super::lua::LuaRepl::new`].
//! 2. Keyword recognition matches Lua 5.4's keyword set
//!    (`local`, `function`, `end`, `if`, `then`, `else`, …) plus the
//!    Lua literals `true` / `false` / `nil`.
//!
//! The scanner is otherwise regex-free: a small character-driven DFA
//! recognises strings, numbers, raw long-bracket strings, the `:meta`
//! prefix, and punctuation. Each token kind maps to a
//! [`nu_ansi_term::Style`].

use std::collections::HashSet;
use std::sync::Arc;

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

/// Token categories recognised by the highlighter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Whitespace,
    /// One of the REPL-installed globals (`session`, `wait`, …).
    Method,
    /// Lua language keyword (`local`, `function`, `end`, …) or literal
    /// (`true`, `false`, `nil`).
    Keyword,
    String,
    Number,
    /// `:meta` prefix at the start of the input line.
    Meta,
    Punctuation,
    Other,
}

/// Lua 5.4 reserved words + the three boolean / nil literals operators
/// usually paint as keywords. `goto` is included for completeness even
/// though the REPL is unlikely to need it.
const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::Whitespace => Style::new(),
        Kind::Method => Color::Cyan.bold(),
        Kind::Keyword => Color::Purple.bold(),
        Kind::String => Color::Green.normal(),
        Kind::Number => Color::LightCyan.normal(),
        Kind::Meta => Color::Magenta.bold(),
        Kind::Punctuation => Style::new().dimmed(),
        Kind::Other => Style::new(),
    }
}

/// reedline-facing highlighter. Holds an `Arc` to the REPL-globals
/// whitelist so the styling tracks installed bindings without per-call
/// cloning.
#[derive(Debug, Clone)]
pub struct ReplHighlighter {
    repl_globals: Arc<HashSet<&'static str>>,
}

impl ReplHighlighter {
    pub fn new(repl_globals: Arc<HashSet<&'static str>>) -> Self {
        Self { repl_globals }
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        for (kind, slice) in scan(line, &self.repl_globals) {
            styled.push((style_for(kind), slice.to_string()));
        }
        styled
    }
}

fn scan<'a>(line: &'a str, repl_globals: &HashSet<&'static str>) -> Vec<(Kind, &'a str)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let ch = bytes[i] as char;

        // `:meta` prefix — only when it's the first non-whitespace token.
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

        // Lua line comments — `--…` to end of line (or `--[[ ... ]]`
        // for blocks; we only handle the line form here, blocks are rare
        // in single-line REPL input).
        if ch == '-' && bytes.get(i + 1).is_some_and(|b| *b as char == '-') {
            while i < bytes.len() && (bytes[i] as char) != '\n' {
                i += 1;
            }
            out.push((Kind::Other, &line[start..i]));
            continue;
        }

        // Single- or double-quoted string literal.
        if ch == '"' || ch == '\'' {
            let quote = ch;
            i += 1;
            while i < bytes.len() {
                let cur = bytes[i] as char;
                if cur == '\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if cur == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push((Kind::String, &line[start..i]));
            continue;
        }

        // Lua long-bracket string `[[...]]`. We don't try to handle
        // arbitrary `[==[ … ]==]` nesting — the basic form is enough
        // for typical REPL input.
        if ch == '[' && bytes.get(i + 1).is_some_and(|b| *b as char == '[') {
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] as char == ']' && bytes[i + 1] as char == ']' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            out.push((Kind::String, &line[start..i]));
            continue;
        }

        if ch.is_ascii_digit()
            || (ch == '-' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()))
        {
            if ch == '-' {
                i += 1;
            }
            while i < bytes.len() {
                let cur = bytes[i] as char;
                if cur.is_ascii_digit() || cur == '.' || cur == 'e' || cur == 'E' {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push((Kind::Number, &line[start..i]));
            continue;
        }

        if is_ident_start(ch) {
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if KEYWORDS.contains(&word) {
                Kind::Keyword
            } else if repl_globals.contains(word) {
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
            '.' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '=' | ':' | ';' | '+' | '-' | '*'
            | '/' | '%' | '<' | '>' => Kind::Punctuation,
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

/// Index of the first non-whitespace byte at or before `i` in `line`.
/// Used to confine the `:meta` prefix rule to the start of the logical
/// input — inside `session.spawn(":foo")` the colon is part of a string,
/// not a meta marker.
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

    fn whitelist(names: &[&'static str]) -> Arc<HashSet<&'static str>> {
        Arc::new(names.iter().copied().collect())
    }

    fn kinds<'a>(line: &'a str, names: &[&'static str]) -> Vec<(Kind, String)> {
        let wl = whitelist(names);
        scan(line, &wl)
            .into_iter()
            .map(|(k, s)| (k, s.to_string()))
            .collect()
    }

    #[test]
    fn highlights_method_chain_and_string_arg() {
        let out = kinds(r#"session.spawn("claude-code")"#, &["session", "spawn"]);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Method && s == "session"),
            "expected `session` highlighted as Method"
        );
        assert!(
            out.iter().any(|(k, s)| *k == Kind::Method && s == "spawn"),
            "expected `spawn` highlighted as Method"
        );
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::String && s == "\"claude-code\""),
            "expected the plugin name as a String token"
        );
    }

    #[test]
    fn highlights_lua_keywords() {
        let out = kinds("local x = 1", &[]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "local"));
        let out = kinds("for i = 1, 5 do end", &[]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "for"));
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "do"));
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "end"));
    }

    #[test]
    fn highlights_true_false_nil_as_keywords() {
        let out = kinds(
            "transcript.snapshot({redact = false})",
            &["transcript", "snapshot"],
        );
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "false"));
        let out = kinds("local x = nil", &[]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Keyword && s == "nil"));
    }

    #[test]
    fn highlights_string_with_escape() {
        let out = kinds(r#"send.text("hi\nthere")"#, &["send", "text"]);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::String && s.starts_with('"') && s.ends_with('"')),
            "expected the escaped string to be one token"
        );
    }

    #[test]
    fn highlights_single_quoted_string() {
        let out = kinds(r#"send.text('hi')"#, &["send", "text"]);
        assert!(
            out.iter().any(|(k, s)| *k == Kind::String && s == "'hi'"),
            "expected single-quoted string token"
        );
    }

    #[test]
    fn highlights_meta_prefix_only_at_start() {
        let out = kinds(":focus e2", &[]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Meta && s == ":focus"));
        // A colon inside a string must not flip the meta highlight on.
        let inside = kinds(r#"send.text(":fake")"#, &["send", "text"]);
        assert!(
            !inside.iter().any(|(k, _)| *k == Kind::Meta),
            "string-internal colon must not be Meta"
        );
    }

    #[test]
    fn unknown_identifier_falls_through_to_other() {
        let out = kinds("flarbnaut(1)", &["session", "spawn"]);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Other && s == "flarbnaut")
        );
    }

    #[test]
    fn negative_integer_keeps_number_kind() {
        let out = kinds("send.intent(\"x\", { count = -5 })", &["send", "intent"]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Number && s == "-5"));
    }

    #[test]
    fn lua_line_comment_does_not_panic() {
        let out = kinds("session.list() -- list adapters", &["session", "list"]);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::Other && s.starts_with("-- "))
        );
    }

    #[test]
    fn long_bracket_string_does_not_panic() {
        let out = kinds(r#"local s = [[hello]]"#, &[]);
        assert!(
            out.iter()
                .any(|(k, s)| *k == Kind::String && s == "[[hello]]")
        );
    }

    #[test]
    fn leading_whitespace_does_not_disable_meta_prefix() {
        let out = kinds("   :tabs", &[]);
        assert!(out.iter().any(|(k, s)| *k == Kind::Meta && s == ":tabs"));
    }

    #[test]
    fn highlighter_handle_returns_styled_text_in_kind_order() {
        let h = ReplHighlighter::new(whitelist(&["session", "spawn"]));
        let styled = h.highlight(r#"session.spawn("x")"#, 0);
        assert!(
            styled.buffer.len() >= 6,
            "expected ≥6 styled spans, got {}",
            styled.buffer.len()
        );
    }

    #[test]
    fn style_for_assigns_distinct_styles_per_kind() {
        // Smoke test the table — if a future contributor adds a Kind
        // and forgets to extend `style_for`, the default `Style::new()`
        // would silently un-style it.
        assert_ne!(style_for(Kind::Method), style_for(Kind::Other));
        assert_ne!(style_for(Kind::String), style_for(Kind::Number));
        assert_ne!(style_for(Kind::Meta), style_for(Kind::Keyword));
        assert_ne!(style_for(Kind::Punctuation), style_for(Kind::Other));
    }
}
