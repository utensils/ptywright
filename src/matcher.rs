use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::ser::{SerializeMap, Serializer};
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
    /// Structured description of the branch that satisfied the matcher.
    /// `None` when the wait surfaced without a successful match (e.g.
    /// `ProcessExited` short-circuit, future cancellation hooks).
    pub outcome: Option<MatchOutcome>,
}

/// Structured description of which matcher branch satisfied a wait.
///
/// Mirrors the [`Matcher`] enum 1:1 so callers can correlate the wait result
/// with the branch they expressed. The serde representation uses a `kind`
/// discriminator (e.g. `{ "kind": "screen_regex", "pattern": "…", "capture":
/// "…" }`) so JSON-RPC consumers can pattern-match on the wire shape directly.
///
/// `Serialize` is implemented by hand rather than derived: serde's tagged-enum
/// derive expands recursive enum variants (`Box<MatchOutcome>` and
/// `Vec<MatchOutcome>` below) into a monomorphization chain that exceeds the
/// default codegen recursion budget. The explicit impl emits the same wire
/// shape without that cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// [`Matcher::ContainsText`] succeeded.
    ContainsText { text: String },
    /// [`Matcher::ScreenRegex`] succeeded. `capture` is the first capture
    /// group when one was declared, otherwise the entire match.
    ScreenRegex {
        pattern: String,
        capture: Option<String>,
    },
    /// [`Matcher::TranscriptContains`] succeeded.
    TranscriptContains { text: String },
    /// [`Matcher::TranscriptRegex`] succeeded. `capture` follows the same
    /// "first group or full match" convention as [`MatchOutcome::ScreenRegex`].
    TranscriptRegex {
        pattern: String,
        capture: Option<String>,
    },
    /// [`Matcher::CursorAt`] succeeded.
    CursorAt { row: u16, col: u16 },
    /// [`Matcher::ScreenStable`] threshold was met.
    ScreenStable { min_ms: u64 },
    /// [`Matcher::ProcessExited`] observed.
    ProcessExited,
    /// [`Matcher::Any`] succeeded — the boxed payload describes which
    /// alternative fired.
    Any(Box<MatchOutcome>),
    /// [`Matcher::All`] succeeded — every nested matcher's outcome in
    /// source order.
    All(Vec<MatchOutcome>),
}

impl Serialize for MatchOutcome {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::ContainsText { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "contains_text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            Self::ScreenRegex { pattern, capture } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "screen_regex")?;
                map.serialize_entry("pattern", pattern)?;
                map.serialize_entry("capture", capture)?;
                map.end()
            }
            Self::TranscriptContains { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "transcript_contains")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            Self::TranscriptRegex { pattern, capture } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "transcript_regex")?;
                map.serialize_entry("pattern", pattern)?;
                map.serialize_entry("capture", capture)?;
                map.end()
            }
            Self::CursorAt { row, col } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "cursor_at")?;
                map.serialize_entry("row", row)?;
                map.serialize_entry("col", col)?;
                map.end()
            }
            Self::ScreenStable { min_ms } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "screen_stable")?;
                map.serialize_entry("min_ms", min_ms)?;
                map.end()
            }
            Self::ProcessExited => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", "process_exited")?;
                map.end()
            }
            Self::Any(inner) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "any")?;
                map.serialize_entry("matched", inner.as_ref())?;
                map.end()
            }
            Self::All(branches) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "all")?;
                map.serialize_entry("matched", branches)?;
                map.end()
            }
        }
    }
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
        self.describe_match(snapshot, transcript).is_some()
    }

    /// Evaluate this matcher with temporal/lifecycle context.
    pub fn is_match_with_context(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
        context: MatcherContext,
    ) -> bool {
        self.describe_match_with_context(snapshot, transcript, context)
            .is_some()
    }

    /// Like [`Matcher::is_match`] but returns a structured
    /// [`MatchOutcome`] describing the branch that fired, for callers (e.g.
    /// `adapter.wait`) that want to know *which* alternation succeeded.
    pub fn describe_match(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
    ) -> Option<MatchOutcome> {
        self.describe_match_with_context(snapshot, transcript, MatcherContext::stateless())
    }

    /// Like [`Matcher::is_match_with_context`] but returns a structured
    /// [`MatchOutcome`] describing the branch that fired.
    pub fn describe_match_with_context(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
        context: MatcherContext,
    ) -> Option<MatchOutcome> {
        match self {
            Self::ContainsText(text) => snapshot
                .plain_text
                .contains(text)
                .then(|| MatchOutcome::ContainsText { text: text.clone() }),
            Self::ScreenRegex(pattern) => {
                cached_regex_capture(pattern, &snapshot.plain_text).map(|capture| {
                    MatchOutcome::ScreenRegex {
                        pattern: pattern.clone(),
                        capture,
                    }
                })
            }
            Self::TranscriptContains(text) => transcript
                .contains(text)
                .then(|| MatchOutcome::TranscriptContains { text: text.clone() }),
            Self::TranscriptRegex(pattern) => {
                cached_regex_capture(pattern, transcript).map(|capture| {
                    MatchOutcome::TranscriptRegex {
                        pattern: pattern.clone(),
                        capture,
                    }
                })
            }
            Self::CursorAt { row, col } => (snapshot.cursor.row == *row
                && snapshot.cursor.col == *col)
                .then_some(MatchOutcome::CursorAt {
                    row: *row,
                    col: *col,
                }),
            Self::ScreenStable { min_ms } => (context.stable_for >= Duration::from_millis(*min_ms))
                .then_some(MatchOutcome::ScreenStable { min_ms: *min_ms }),
            Self::ProcessExited => context
                .process_exited
                .then_some(MatchOutcome::ProcessExited),
            Self::Any(matchers) => matchers
                .iter()
                .find_map(|matcher| {
                    matcher.describe_match_with_context(snapshot, transcript, context)
                })
                .map(|inner| MatchOutcome::Any(Box::new(inner))),
            Self::All(matchers) => {
                let mut outcomes = Vec::with_capacity(matchers.len());
                for matcher in matchers {
                    let outcome =
                        matcher.describe_match_with_context(snapshot, transcript, context)?;
                    outcomes.push(outcome);
                }
                Some(MatchOutcome::All(outcomes))
            }
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

/// Return the matching slice (first capture group when one is declared,
/// otherwise the whole match) when `pattern` matches `text`. Shares the regex
/// cache with [`Matcher::is_match_with_context`]'s underlying compiled regex
/// pool so callers paying for a structured outcome don't double-compile the
/// pattern.
fn cached_regex_capture(pattern: &str, text: &str) -> Option<Option<String>> {
    with_cached_regex(pattern, |regex| {
        regex.captures(text).map(|captures| {
            let group = captures.get(1).or_else(|| captures.get(0));
            group.map(|m| m.as_str().to_string())
        })
    })
}

/// Shared cache front-end: compile-on-first-use, evict on overflow, then run
/// the caller's reader against the cached regex (or against the absence of one
/// if compilation failed). Centralises the lock and the cap so the two cached
/// entry points cannot drift.
fn with_cached_regex<T: Default, F>(pattern: &str, f: F) -> T
where
    F: FnOnce(&Regex) -> T,
{
    const MAX_REGEX_CACHE_ENTRIES: usize = 128;

    static REGEX_CACHE: OnceLock<Mutex<HashMap<String, Option<Regex>>>> = OnceLock::new();
    let cache = REGEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("regex cache lock poisoned");
    if let Some(regex) = cache.get(pattern) {
        return regex.as_ref().map(f).unwrap_or_default();
    }

    let regex = Regex::new(pattern).ok();
    let result = regex.as_ref().map(f).unwrap_or_default();
    if cache.len() >= MAX_REGEX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(pattern.to_string(), regex);
    result
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

    #[test]
    fn describe_match_returns_contains_text_branch() {
        let snap = snapshot("hello screen");
        let outcome = Matcher::ContainsText("screen".into()).describe_match(&snap, "");
        assert!(matches!(
            outcome,
            Some(MatchOutcome::ContainsText { ref text }) if text == "screen"
        ));
        assert!(
            Matcher::ContainsText("missing".into())
                .describe_match(&snap, "")
                .is_none()
        );
    }

    #[test]
    fn describe_match_captures_screen_regex_capture_group() {
        let snap = snapshot("user=jdoe ready");
        let outcome = Matcher::ScreenRegex(r"user=(\w+)".into()).describe_match(&snap, "");
        let Some(MatchOutcome::ScreenRegex { pattern, capture }) = outcome else {
            panic!("expected ScreenRegex outcome; got {outcome:?}");
        };
        assert_eq!(pattern, r"user=(\w+)");
        assert_eq!(capture.as_deref(), Some("jdoe"));
    }

    #[test]
    fn describe_match_screen_regex_without_capture_group_returns_full_match() {
        let snap = snapshot("READY");
        let outcome = Matcher::ScreenRegex("REA.Y".into()).describe_match(&snap, "");
        let Some(MatchOutcome::ScreenRegex { capture, .. }) = outcome else {
            panic!("expected ScreenRegex outcome; got {outcome:?}");
        };
        assert_eq!(capture.as_deref(), Some("READY"));
    }

    #[test]
    fn describe_match_transcript_regex_extracts_capture() {
        let snap = snapshot("");
        let outcome = Matcher::TranscriptRegex(r"cost=(\$[0-9.]+)".into())
            .describe_match(&snap, "cost=$1.23 done");
        let Some(MatchOutcome::TranscriptRegex { capture, .. }) = outcome else {
            panic!("expected TranscriptRegex outcome; got {outcome:?}");
        };
        assert_eq!(capture.as_deref(), Some("$1.23"));
    }

    #[test]
    fn describe_match_cursor_at_carries_position() {
        let snap = snapshot("");
        let outcome = Matcher::CursorAt { row: 1, col: 2 }.describe_match(&snap, "");
        assert!(matches!(
            outcome,
            Some(MatchOutcome::CursorAt { row: 1, col: 2 })
        ));
    }

    #[test]
    fn describe_match_screen_stable_reports_threshold() {
        let snap = snapshot("ready");
        let outcome = Matcher::ScreenStable { min_ms: 200 }.describe_match_with_context(
            &snap,
            "",
            MatcherContext {
                stable_for: Duration::from_millis(250),
                process_exited: false,
            },
        );
        assert!(matches!(
            outcome,
            Some(MatchOutcome::ScreenStable { min_ms: 200 })
        ));
    }

    #[test]
    fn describe_match_any_reports_which_branch_fired() {
        let snap = snapshot("ready");
        let outcome = Matcher::Any(vec![
            Matcher::ContainsText("no".into()),
            Matcher::ContainsText("ready".into()),
        ])
        .describe_match(&snap, "");
        let Some(MatchOutcome::Any(inner)) = outcome else {
            panic!("expected Any outcome; got {outcome:?}");
        };
        assert!(matches!(*inner, MatchOutcome::ContainsText { ref text } if text == "ready"));
    }

    #[test]
    fn describe_match_all_collects_every_branch_outcome() {
        let snap = snapshot("ready");
        let outcome = Matcher::All(vec![
            Matcher::ContainsText("rea".into()),
            Matcher::ContainsText("dy".into()),
        ])
        .describe_match(&snap, "");
        let Some(MatchOutcome::All(branches)) = outcome else {
            panic!("expected All outcome; got {outcome:?}");
        };
        assert_eq!(branches.len(), 2);
        assert!(matches!(branches[0], MatchOutcome::ContainsText { ref text } if text == "rea"));
        assert!(matches!(branches[1], MatchOutcome::ContainsText { ref text } if text == "dy"));
    }

    #[test]
    fn describe_match_outcome_serialises_with_kind_tag() {
        let outcome = MatchOutcome::ScreenRegex {
            pattern: "rea.y".to_string(),
            capture: Some("ready".to_string()),
        };
        let json = serde_json::to_value(&outcome).expect("serialize outcome");
        assert_eq!(json["kind"], "screen_regex");
        assert_eq!(json["pattern"], "rea.y");
        assert_eq!(json["capture"], "ready");
    }
}
