use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
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

/// Object-safe trait for evaluating plugin-defined predicates against a
/// terminal session. Implementors hold whatever runtime state is
/// necessary to invoke a named predicate function on a named plugin —
/// typically a map from plugin name to an `Arc<Mutex<LuaPlugin>>`.
///
/// `Matcher::Lua { plugin, predicate, params }` defers to a bound
/// registry at evaluation time. Plain string fields on the matcher
/// keep [`Matcher`] fully serializable; the trait object lives on the
/// session (see [`crate::Session::set_plugin_registry`]) rather than
/// inside the matcher.
///
/// The trait is domain-neutral — it takes a string plugin name and a
/// string predicate name. Any TUI plugin's predicates work through the
/// same path; the host does not bake in knowledge of which plugins
/// exist.
pub trait PluginRegistry: Send + Sync {
    /// Invoke `predicate` on `plugin` with the supplied `params` and
    /// terminal `context`. Implementors return the predicate's
    /// structured outcome (matched + optional evidence / capture).
    ///
    /// Errors surface as [`crate::Error::Lua`] (or any matching crate
    /// error variant) when the plugin doesn't exist, the predicate
    /// isn't exported, or the call faults — the wait loop treats
    /// errors as "predicate did not fire" rather than aborting the
    /// wait, so a transient plugin fault leaves the wait running for
    /// the next tick instead of poisoning the whole operation.
    ///
    /// **Determinism contract.** Predicates must return a stable
    /// outcome for a given `(plugin, predicate, params, context)`
    /// tuple — the wait loop is allowed to call `evaluate_predicate`
    /// twice on the same tick when assembling the structured
    /// [`MatchOutcome`]: once for the cheap boolean check and once
    /// for the outcome construction. A predicate with side effects
    /// or non-determinism (RNG, clock-based branching, mutating Lua
    /// state) may report `true` on the first call and `false` on the
    /// second, producing a successful wait whose `outcome` field is
    /// `None`. Plugin authors: treat predicates as pure inspections
    /// of the supplied context.
    fn evaluate_predicate(
        &self,
        plugin: &str,
        predicate: &str,
        params: &Value,
        context: &PredicateContext<'_>,
    ) -> Result<PredicateOutcome>;
}

/// Context handed to a [`PluginRegistry`] implementation when
/// evaluating a `Matcher::Lua` predicate.
///
/// Reference-bound to avoid per-tick cloning on the hot wait path.
/// Predicates that need a body/status split (a plugin-specific
/// convention) can compute it themselves from `screen` — the core
/// matcher layer stays neutral on whether such a split exists.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PredicateContext<'a> {
    /// Full visible screen text.
    pub screen: &'a str,
    /// Retained transcript tail.
    pub transcript: &'a str,
    /// Session sequence observed when this context was assembled.
    pub sequence: u64,
    /// How long the current screen has been stable, in milliseconds.
    pub stable_ms: u64,
    /// Whether the PTY/session lifecycle indicates the process has
    /// exited or closed.
    pub process_exited: bool,
    /// All currently-recorded transcript markers — same view the
    /// classifier sees through [`crate::ClassifyContext::markers`].
    pub markers: &'a BTreeMap<String, u64>,
    /// Current transcript `chars_written` cursor.
    pub cursor: u64,
}

/// Structured result returned by a [`PluginRegistry`] for a single
/// predicate evaluation. The host turns `matched = true` into a wait
/// completion and surfaces `evidence` / `capture` (when present) on
/// the resulting [`MatchOutcome::Lua`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateOutcome {
    /// Whether the predicate fires this tick.
    pub matched: bool,
    /// Optional human-readable evidence string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Optional captured substring (analogous to a regex first
    /// capture). Predicates that report a meaningful payload populate
    /// this so callers can correlate the match with the substance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
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
    /// [`Session::wait_for`](crate::session::Session::wait_for) only
    /// constructs `MatchResult` on a successful match today, so this is
    /// always `Some(_)` in practice — the `Option` wrapper keeps the field
    /// future-proof for cancellation hooks that might surface a non-match
    /// `MatchResult` (e.g. on timeout) without requiring a wire schema
    /// migration.
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
    /// group when one was declared, otherwise the entire match. The host
    /// always populates this on success — pattern compilation failure is
    /// reported via a missing outcome (the matcher fails to fire), not by
    /// emitting a `ScreenRegex` outcome without a capture.
    ScreenRegex { pattern: String, capture: String },
    /// [`Matcher::TranscriptContains`] succeeded.
    TranscriptContains { text: String },
    /// [`Matcher::TranscriptRegex`] succeeded. `capture` follows the same
    /// "first group or full match" convention as [`MatchOutcome::ScreenRegex`]
    /// and is always populated on success.
    TranscriptRegex { pattern: String, capture: String },
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
    /// [`Matcher::Lua`] predicate fired. `evidence` and `capture` are
    /// the optional fields the plugin populated on its
    /// [`PredicateOutcome`] return value.
    Lua {
        plugin: String,
        predicate: String,
        evidence: Option<String>,
        capture: Option<String>,
    },
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
            Self::Lua {
                plugin,
                predicate,
                evidence,
                capture,
            } => {
                let len = 3 + usize::from(evidence.is_some()) + usize::from(capture.is_some());
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("kind", "lua")?;
                map.serialize_entry("plugin", plugin)?;
                map.serialize_entry("predicate", predicate)?;
                if let Some(value) = evidence {
                    map.serialize_entry("evidence", value)?;
                }
                if let Some(value) = capture {
                    map.serialize_entry("capture", value)?;
                }
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
    /// A plugin-defined predicate evaluated by the bound
    /// [`PluginRegistry`]. Domain-neutral: any plugin's exported
    /// predicate function works through the same wire shape.
    ///
    /// Requires a registry bound on the [`crate::Session`] via
    /// [`crate::Session::set_plugin_registry`]. Without a registry,
    /// evaluation through [`Matcher::is_match_with_context`] (the
    /// stateless API) returns `false` — call
    /// [`Matcher::is_match_with_evaluator`] for the registry-aware
    /// path.
    Lua {
        /// Plugin name (manifest `name` field).
        plugin: String,
        /// Predicate function name exported by the plugin's Lua source.
        predicate: String,
        /// JSON params forwarded to the predicate. Omitted on the
        /// wire when null.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
    },
}

impl Matcher {
    /// Evaluate this matcher against a screen snapshot and transcript text.
    pub fn is_match(&self, snapshot: &ScreenSnapshot, transcript: &str) -> bool {
        self.is_match_with_context(snapshot, transcript, MatcherContext::stateless())
    }

    /// Evaluate this matcher with temporal/lifecycle context.
    ///
    /// Uses an allocation-free fast path so callers that only need a
    /// boolean check (the common case for [`Session::wait_for`] inner
    /// polling) don't pay the per-iteration cost of building a
    /// [`MatchOutcome`] — that work happens once at success time through
    /// [`Matcher::describe_match_with_context`] instead.
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
            // `Matcher::Lua` requires a bound `PluginRegistry`. The
            // stateless API cannot evaluate it; callers wanting the
            // plugin-driven path go through `is_match_with_evaluator`.
            // Returning `false` here keeps existing call sites working
            // without surprising panics if a Lua matcher leaks into a
            // registry-less wait path.
            Self::Lua { .. } => false,
        }
    }

    /// Like [`Matcher::is_match_with_context`] but also evaluates
    /// [`Matcher::Lua`] branches against the supplied registry +
    /// predicate context. Non-Lua variants ignore the registry and
    /// delegate to [`Matcher::is_match_with_context`].
    ///
    /// The wait loop on [`crate::Session`] uses this method whenever a
    /// plugin registry is bound; the predicate context is rebuilt
    /// every tick from the current screen / transcript / markers.
    pub fn is_match_with_evaluator(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
        context: MatcherContext,
        predicate_context: &PredicateContext<'_>,
        registry: Option<&dyn PluginRegistry>,
    ) -> bool {
        match self {
            Self::Lua {
                plugin,
                predicate,
                params,
            } => registry
                .and_then(|r| {
                    r.evaluate_predicate(plugin, predicate, params, predicate_context)
                        .ok()
                })
                .map(|outcome| outcome.matched)
                .unwrap_or(false),
            Self::Any(matchers) => matchers.iter().any(|matcher| {
                matcher.is_match_with_evaluator(
                    snapshot,
                    transcript,
                    context,
                    predicate_context,
                    registry,
                )
            }),
            Self::All(matchers) => matchers.iter().all(|matcher| {
                matcher.is_match_with_evaluator(
                    snapshot,
                    transcript,
                    context,
                    predicate_context,
                    registry,
                )
            }),
            _ => self.is_match_with_context(snapshot, transcript, context),
        }
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
            // No registry available → no outcome. Use
            // `describe_match_with_evaluator` for the plugin-driven
            // path (see [`Matcher::is_match_with_context`] for the
            // analogous fallthrough).
            Self::Lua { .. } => None,
        }
    }

    /// Like [`Matcher::describe_match_with_context`] but evaluates
    /// [`Matcher::Lua`] branches against the supplied registry.
    /// Returns the same outcomes for non-Lua variants;
    /// `Matcher::Lua` fires a [`MatchOutcome::Lua`] populated from
    /// the registry's [`PredicateOutcome`].
    pub fn describe_match_with_evaluator(
        &self,
        snapshot: &ScreenSnapshot,
        transcript: &str,
        context: MatcherContext,
        predicate_context: &PredicateContext<'_>,
        registry: Option<&dyn PluginRegistry>,
    ) -> Option<MatchOutcome> {
        match self {
            Self::Lua {
                plugin,
                predicate,
                params,
            } => {
                let outcome = registry.and_then(|r| {
                    r.evaluate_predicate(plugin, predicate, params, predicate_context)
                        .ok()
                })?;
                outcome.matched.then_some(MatchOutcome::Lua {
                    plugin: plugin.clone(),
                    predicate: predicate.clone(),
                    evidence: outcome.evidence,
                    capture: outcome.capture,
                })
            }
            Self::Any(matchers) => matchers
                .iter()
                .find_map(|matcher| {
                    matcher.describe_match_with_evaluator(
                        snapshot,
                        transcript,
                        context,
                        predicate_context,
                        registry,
                    )
                })
                .map(|inner| MatchOutcome::Any(Box::new(inner))),
            Self::All(matchers) => {
                let mut outcomes = Vec::with_capacity(matchers.len());
                for matcher in matchers {
                    let outcome = matcher.describe_match_with_evaluator(
                        snapshot,
                        transcript,
                        context,
                        predicate_context,
                        registry,
                    )?;
                    outcomes.push(outcome);
                }
                Some(MatchOutcome::All(outcomes))
            }
            _ => self.describe_match_with_context(snapshot, transcript, context),
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

/// Allocation-free boolean check used by [`Matcher::is_match_with_context`]
/// to keep the polling path cheap when no outcome metadata is needed.
fn cached_regex_is_match(pattern: &str, text: &str) -> bool {
    with_cached_regex(pattern, |regex| regex.is_match(text))
}

/// Return the matching slice (first capture group when one is declared,
/// otherwise the whole match) when `pattern` matches `text`. Shares the regex
/// cache with [`cached_regex_is_match`] so callers paying for a structured
/// outcome don't double-compile the pattern.
///
/// The inner unwrap is safe: a successful `Regex::captures` always yields
/// group 0 (the full match), and we fall back to it when no named group 1
/// was declared. Returning a flat `Option<String>` avoids forcing callers
/// to handle an "outer Some, inner None" case that cannot occur.
fn cached_regex_capture(pattern: &str, text: &str) -> Option<String> {
    with_cached_regex(pattern, |regex| {
        regex.captures(text).map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(0))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default()
        })
    })
}

/// Shared cache front-end: compile-on-first-use, evict on overflow, then run
/// the caller's reader against the cached regex (or against the absence of one
/// if compilation failed). Centralises the lock and the cap so
/// [`cached_regex_is_match`] (the allocation-free boolean fast path) and
/// [`cached_regex_capture`] (the structured-outcome path) cannot drift.
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
        assert_eq!(capture, "jdoe");
    }

    #[test]
    fn describe_match_screen_regex_without_capture_group_returns_full_match() {
        let snap = snapshot("READY");
        let outcome = Matcher::ScreenRegex("REA.Y".into()).describe_match(&snap, "");
        let Some(MatchOutcome::ScreenRegex { capture, .. }) = outcome else {
            panic!("expected ScreenRegex outcome; got {outcome:?}");
        };
        assert_eq!(capture, "READY");
    }

    #[test]
    fn describe_match_transcript_regex_extracts_capture() {
        let snap = snapshot("");
        let outcome = Matcher::TranscriptRegex(r"cost=(\$[0-9.]+)".into())
            .describe_match(&snap, "cost=$1.23 done");
        let Some(MatchOutcome::TranscriptRegex { capture, .. }) = outcome else {
            panic!("expected TranscriptRegex outcome; got {outcome:?}");
        };
        assert_eq!(capture, "$1.23");
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
            capture: "ready".to_string(),
        };
        let json = serde_json::to_value(&outcome).expect("serialize outcome");
        assert_eq!(json["kind"], "screen_regex");
        assert_eq!(json["pattern"], "rea.y");
        assert_eq!(json["capture"], "ready");
    }

    /// Minimal `PluginRegistry` impl that returns a fixed outcome
    /// regardless of input. Used by the Lua-matcher tests below — we
    /// don't need a real Lua state to verify the threading through
    /// `is_match_with_evaluator`.
    struct FixedOutcomeRegistry {
        outcome: PredicateOutcome,
        expected_plugin: &'static str,
        expected_predicate: &'static str,
    }

    impl PluginRegistry for FixedOutcomeRegistry {
        fn evaluate_predicate(
            &self,
            plugin: &str,
            predicate: &str,
            _params: &Value,
            _context: &PredicateContext<'_>,
        ) -> crate::error::Result<PredicateOutcome> {
            assert_eq!(plugin, self.expected_plugin);
            assert_eq!(predicate, self.expected_predicate);
            Ok(self.outcome.clone())
        }
    }

    #[test]
    fn matcher_lua_serializes_with_struct_value_payload() {
        let matcher = Matcher::Lua {
            plugin: "claude-code".into(),
            predicate: "is_settled".into(),
            params: serde_json::json!({ "anchor": "ready" }),
        };
        let wire = serde_json::to_value(&matcher).expect("serialize Matcher::Lua");
        assert_eq!(wire["type"], "lua");
        assert_eq!(wire["value"]["plugin"], "claude-code");
        assert_eq!(wire["value"]["predicate"], "is_settled");
        assert_eq!(wire["value"]["params"]["anchor"], "ready");
    }

    #[test]
    fn matcher_lua_round_trips_through_serde() {
        let original = Matcher::Lua {
            plugin: "demo".into(),
            predicate: "ready".into(),
            params: serde_json::json!({}),
        };
        let wire = serde_json::to_string(&original).expect("serialize");
        let parsed: Matcher = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn is_match_with_context_returns_false_for_lua_without_registry() {
        // The stateless API cannot evaluate plugin predicates — the
        // registry-less path must return false (not panic) so existing
        // call sites keep working if a Lua matcher leaks in.
        let snap = snapshot("anything");
        let matcher = Matcher::Lua {
            plugin: "x".into(),
            predicate: "y".into(),
            params: serde_json::Value::Null,
        };
        assert!(!matcher.is_match(&snap, ""));
        assert!(matcher.describe_match(&snap, "").is_none());
    }

    #[test]
    fn is_match_with_evaluator_consults_registry_for_lua() {
        let snap = snapshot("session ready");
        let registry = FixedOutcomeRegistry {
            outcome: PredicateOutcome {
                matched: true,
                evidence: Some("plugin saw ready".into()),
                capture: Some("ready".into()),
            },
            expected_plugin: "demo",
            expected_predicate: "is_ready",
        };
        let markers = BTreeMap::new();
        let predicate_ctx = PredicateContext {
            screen: "session ready",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let matcher = Matcher::Lua {
            plugin: "demo".into(),
            predicate: "is_ready".into(),
            params: serde_json::Value::Null,
        };
        assert!(matcher.is_match_with_evaluator(
            &snap,
            "",
            MatcherContext::stateless(),
            &predicate_ctx,
            Some(&registry),
        ));
    }

    #[test]
    fn describe_match_with_evaluator_returns_lua_outcome() {
        let snap = snapshot("anything");
        let registry = FixedOutcomeRegistry {
            outcome: PredicateOutcome {
                matched: true,
                evidence: Some("hit".into()),
                capture: Some("captured".into()),
            },
            expected_plugin: "demo",
            expected_predicate: "fires",
        };
        let markers = BTreeMap::new();
        let predicate_ctx = PredicateContext {
            screen: "anything",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let matcher = Matcher::Lua {
            plugin: "demo".into(),
            predicate: "fires".into(),
            params: serde_json::Value::Null,
        };
        let outcome = matcher.describe_match_with_evaluator(
            &snap,
            "",
            MatcherContext::stateless(),
            &predicate_ctx,
            Some(&registry),
        );
        match outcome {
            Some(MatchOutcome::Lua {
                plugin,
                predicate,
                evidence,
                capture,
            }) => {
                assert_eq!(plugin, "demo");
                assert_eq!(predicate, "fires");
                assert_eq!(evidence.as_deref(), Some("hit"));
                assert_eq!(capture.as_deref(), Some("captured"));
            }
            other => panic!("expected MatchOutcome::Lua; got {other:?}"),
        }
    }

    #[test]
    fn describe_match_outcome_lua_serialises_with_kind_tag() {
        let outcome = MatchOutcome::Lua {
            plugin: "p".into(),
            predicate: "q".into(),
            evidence: Some("note".into()),
            capture: None,
        };
        let json = serde_json::to_value(&outcome).expect("serialize Lua outcome");
        assert_eq!(json["kind"], "lua");
        assert_eq!(json["plugin"], "p");
        assert_eq!(json["predicate"], "q");
        assert_eq!(json["evidence"], "note");
        assert!(json.get("capture").is_none(), "absent fields stay absent");
    }

    #[test]
    fn evaluator_combinators_compose_with_lua_branches() {
        // Any/All combinators must recurse into Lua branches via the
        // evaluator-aware path. Mixing native and Lua matchers is the
        // typical real-world shape ("wait for the screen to stabilise
        // AND for the plugin's custom predicate to fire").
        let snap = snapshot("waiting…");
        let registry = FixedOutcomeRegistry {
            outcome: PredicateOutcome {
                matched: true,
                evidence: None,
                capture: None,
            },
            expected_plugin: "demo",
            expected_predicate: "ok",
        };
        let markers = BTreeMap::new();
        let predicate_ctx = PredicateContext {
            screen: "waiting…",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let matcher = Matcher::All(vec![
            Matcher::ContainsText("waiting".into()),
            Matcher::Lua {
                plugin: "demo".into(),
                predicate: "ok".into(),
                params: serde_json::Value::Null,
            },
        ]);
        assert!(matcher.is_match_with_evaluator(
            &snap,
            "",
            MatcherContext::stateless(),
            &predicate_ctx,
            Some(&registry),
        ));
    }
}
