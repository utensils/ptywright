use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::screen::ScreenSnapshot;

/// Evidence returned from a successful or failed wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// Whether the matcher succeeded.
    pub matched: bool,
    /// Session sequence observed when the wait finished.
    pub sequence: u64,
    /// Elapsed wait time.
    pub elapsed: Duration,
    /// Screen snapshot used for the final decision.
    pub snapshot: ScreenSnapshot,
    /// Tail of the transcript used for the final decision.
    pub transcript_tail: String,
}

/// Predicate for screen/transcript state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Matcher {
    /// Visible screen contains the provided text.
    ContainsText(String),
    /// Visible screen matches the provided regular expression.
    ScreenRegex(String),
    /// Retained transcript contains the provided text.
    TranscriptContains(String),
    /// Retained transcript tail matches the provided regular expression.
    TranscriptRegex(String),
    /// Cursor is at the provided zero-based position.
    CursorAt { row: u16, col: u16 },
    /// Any nested matcher succeeds.
    Any(Vec<Matcher>),
    /// All nested matchers succeed.
    All(Vec<Matcher>),
}

impl Matcher {
    /// Evaluate this matcher against a screen snapshot and transcript text.
    pub fn is_match(&self, snapshot: &ScreenSnapshot, transcript: &str) -> bool {
        match self {
            Self::ContainsText(text) => snapshot.plain_text.contains(text),
            Self::ScreenRegex(pattern) => Regex::new(pattern)
                .map(|regex| regex.is_match(&snapshot.plain_text))
                .unwrap_or(false),
            Self::TranscriptContains(text) => transcript.contains(text),
            Self::TranscriptRegex(pattern) => Regex::new(pattern)
                .map(|regex| regex.is_match(transcript))
                .unwrap_or(false),
            Self::CursorAt { row, col } => {
                snapshot.cursor.row == *row && snapshot.cursor.col == *col
            }
            Self::Any(matchers) => matchers
                .iter()
                .any(|matcher| matcher.is_match(snapshot, transcript)),
            Self::All(matchers) => matchers
                .iter()
                .all(|matcher| matcher.is_match(snapshot, transcript)),
        }
    }
}

impl From<&str> for Matcher {
    fn from(value: &str) -> Self {
        Self::ContainsText(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::screen::CursorState;
    use crate::target::TerminalSize;

    use super::*;

    fn snapshot(text: &str) -> ScreenSnapshot {
        ScreenSnapshot {
            size: TerminalSize::new(3, 20),
            cursor: CursorState {
                row: 1,
                col: 2,
                visible: true,
            },
            sequence: 9,
            plain_text: text.to_string(),
        }
    }

    #[test]
    fn matchers_evaluate_screen_and_transcript() {
        let snapshot = snapshot("hello screen");

        assert!(Matcher::ContainsText("screen".into()).is_match(&snapshot, "log tail"));
        assert!(Matcher::ScreenRegex("h.llo".into()).is_match(&snapshot, ""));
        assert!(Matcher::TranscriptContains("tail".into()).is_match(&snapshot, "log tail"));
        assert!(Matcher::CursorAt { row: 1, col: 2 }.is_match(&snapshot, ""));
    }

    #[test]
    fn invalid_regex_does_not_match() {
        assert!(!Matcher::ScreenRegex("[".into()).is_match(&snapshot("text"), ""));
    }

    #[test]
    fn any_and_all_combine_matchers() {
        let snapshot = snapshot("ready");

        assert!(
            Matcher::Any(vec![
                Matcher::ContainsText("no".into()),
                Matcher::ContainsText("ready".into())
            ])
            .is_match(&snapshot, "")
        );
        assert!(
            Matcher::All(vec![
                Matcher::ContainsText("rea".into()),
                Matcher::ContainsText("ady".into())
            ])
            .is_match(&snapshot, "")
        );
    }
}
