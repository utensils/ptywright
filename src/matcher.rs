use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::screen::ScreenSnapshot;

/// Runtime context for temporal/lifecycle matcher evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatcherContext {
    /// How long the current screen sequence has been stable.
    pub stable_for: Duration,
    /// Whether the PTY/session lifecycle indicates the process has exited or closed.
    pub process_exited: bool,
}

impl MatcherContext {
    /// Context for pure snapshot/transcript matcher evaluation.
    #[must_use]
    pub const fn stateless() -> Self {
        Self {
            stable_for: Duration::ZERO,
            process_exited: false,
        }
    }
}

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
    /// How long the current screen sequence had been stable when the matcher
    /// fired. Forwarded by [`Session::wait_for`](crate::session::Session::wait_for)
    /// so classifiers can reason about actual stability rather than a
    /// configured threshold — important for adapters whose wait matcher does
    /// not include `screen_stable`.
    pub stable_for: Duration,
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
    /// Current screen sequence has been unchanged for at least this duration.
    ScreenStable { min_ms: u64 },
    /// Session lifecycle indicates the child process has exited or closed.
    ProcessExited,
    /// Any nested matcher succeeds.
    Any(Vec<Matcher>),
    /// All nested matchers succeed.
    All(Vec<Matcher>),
}

impl Matcher {
    /// Evaluate this matcher against a screen snapshot and transcript text.
    pub fn is_match(&self, snapshot: &ScreenSnapshot, transcript: &str) -> bool {
        self.is_match_with_context(snapshot, transcript, MatcherContext::stateless())
    }

    /// Evaluate this matcher with temporal/lifecycle context.
    pub fn is_match_with_context(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
        context: MatcherContext,
    ) -> bool {
        match self {
            Self::ContainsText(text) => snapshot.plain_text.contains(text),
            Self::ScreenRegex(pattern) => cached_regex_is_match(pattern, &snapshot.plain_text),
            Self::TranscriptContains(text) => transcript.contains(text),
            Self::TranscriptRegex(pattern) => cached_regex_is_match(pattern, transcript),
            Self::CursorAt { row, col } => {
                snapshot.cursor.row == *row && snapshot.cursor.col == *col
            }
            Self::ScreenStable { min_ms } => context.stable_for >= Duration::from_millis(*min_ms),
            Self::ProcessExited => context.process_exited,
            Self::Any(matchers) => matchers
                .iter()
                .any(|matcher| matcher.is_match_with_context(snapshot, transcript, context)),
            Self::All(matchers) => matchers
                .iter()
                .all(|matcher| matcher.is_match_with_context(snapshot, transcript, context)),
        }
    }

    /// Minimum stable-screen duration required anywhere in this matcher tree.
    #[must_use]
    pub fn minimum_stable_duration(&self) -> Option<Duration> {
        match self {
            Self::ScreenStable { min_ms } => Some(Duration::from_millis(*min_ms)),
            Self::Any(matchers) | Self::All(matchers) => matchers
                .iter()
                .filter_map(Self::minimum_stable_duration)
                .min(),
            _ => None,
        }
    }
}

fn cached_regex_is_match(pattern: &str, text: &str) -> bool {
    const MAX_REGEX_CACHE_ENTRIES: usize = 128;

    static REGEX_CACHE: OnceLock<Mutex<HashMap<String, Option<Regex>>>> = OnceLock::new();
    let cache = REGEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("regex cache lock poisoned");
    if let Some(regex) = cache.get(pattern) {
        return regex.as_ref().is_some_and(|regex| regex.is_match(text));
    }

    let regex = Regex::new(pattern).ok();
    let matched = regex.as_ref().is_some_and(|regex| regex.is_match(text));
    if cache.len() >= MAX_REGEX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(pattern.to_string(), regex);
    matched
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
            cells: Vec::new(),
            alternate_screen: false,
            application_cursor: false,
            application_keypad: false,
            title: None,
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
    fn repeated_regex_uses_cached_result() {
        let matcher = Matcher::ScreenRegex("rea.y".into());

        assert!(matcher.is_match(&snapshot("ready"), ""));
        assert!(matcher.is_match(&snapshot("ready"), ""));
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

    #[test]
    fn temporal_matchers_use_context() {
        let snapshot = snapshot("ready");
        let context = MatcherContext {
            stable_for: Duration::from_millis(300),
            process_exited: true,
        };

        assert!(
            Matcher::ScreenStable { min_ms: 250 }.is_match_with_context(&snapshot, "", context)
        );
        assert!(Matcher::ProcessExited.is_match_with_context(&snapshot, "", context));
        assert!(
            !Matcher::ScreenStable { min_ms: 500 }.is_match_with_context(&snapshot, "", context)
        );
    }
}
