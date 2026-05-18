//! Generic extension layer that lets adapters plug into ptywright through a
//! small, runtime-agnostic trait.
//!
//! The [`Extension`] trait abstracts whatever produces classifier output and
//! action plans for an interactive TUI. [`LuaExtension`] is the only
//! implementor shipped today; a future WASM or external-process plugin can
//! drop in by implementing this trait without changing the rest of the core.
//!
//! Everything here is intentionally application-agnostic: no Claude-specific
//! state names, no Claude-specific intents. Application-specific behaviour
//! lives entirely in Lua plugins under `plugins/<name>/` — ptywright does
//! not carry typed Rust shims per TUI. Callers in Rust that want a typed
//! enum can convert plugin state strings on their own.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::Action;
use crate::error::{Error, Result};
use crate::lua_plugin::LuaPlugin;
use crate::matcher::{MatchOutcome, Matcher};
use crate::plugin::{BUILTIN_PLUGINS, PluginManifest};
use crate::session::Session;

/// Bottom rows of a rendered screen treated as the status bar.
///
/// Used to split screen text into body and status before handing it to a
/// classifier so that benign status strings cannot false-positive on
/// substring matches in the body classifier (e.g. `bypass permissions on`).
pub const STATUS_BAR_ROWS: usize = 3;

/// Snapshot of an extension's classified state.
///
/// The `state` field is a plugin-defined string so the generic core does not
/// have to know any specific plugin's vocabulary. Plugins return a
/// confidence score in `[0.0, 1.0]`, a human-readable `evidence` string, the
/// session `sequence` observed at classification time, and an optional list
/// of ranked runner-up `candidates`.
///
/// `#[non_exhaustive]` is set so future plugin-driven additions (richer
/// candidate metadata, classifier latency, …) can land without
/// source-breaking downstream Rust callers. From outside the crate, build
/// via [`ExtensionStateSnapshot::new`] (which seeds the required `state` /
/// `sequence` fields and leaves the rest at sensible defaults) and then
/// assign whichever public fields you want to override:
///
/// ```ignore
/// let mut snap = ExtensionStateSnapshot::new("ready", 42);
/// snap.confidence = 0.95;
/// snap.evidence = "ready prompt visible".into();
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExtensionStateSnapshot {
    /// Plugin-defined classification, e.g. `"ready"`, `"thinking"`, etc.
    pub state: String,
    /// Confidence from 0.0 to 1.0.
    #[serde(serialize_with = "serialize_confidence")]
    pub confidence: f32,
    /// Human-readable evidence used for the classification.
    pub evidence: String,
    /// Session sequence observed for the classification.
    pub sequence: u64,
    /// Runner-up classifications, ranked by confidence descending.
    ///
    /// Empty until plugins start populating it; safe to ignore.
    #[serde(default)]
    pub candidates: Vec<StateCandidate>,
    /// Opaque plugin-defined metadata attached to this classification.
    ///
    /// The host does not interpret it — plugins shape it however they like so
    /// callers can read structured fields parsed from the screen (cost,
    /// usage, model, context-window stats, permission-dialog detail, …)
    /// without re-scraping. Omitted on the wire when empty so the common
    /// "no metadata" case stays cheap to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ExtensionStateSnapshot {
    /// Construct a snapshot with the required `state` label and observed
    /// `sequence`. All other public fields land at their defaults (zero
    /// confidence, empty evidence, empty candidates, no metadata) — set
    /// them by assigning the public fields after construction. The struct
    /// is `#[non_exhaustive]` so this constructor is the only way to
    /// build one from outside the crate.
    #[must_use]
    pub fn new(state: impl Into<String>, sequence: u64) -> Self {
        Self {
            state: state.into(),
            confidence: 0.0,
            evidence: String::new(),
            sequence,
            candidates: Vec::new(),
            metadata: None,
        }
    }
}

/// Runner-up classification produced alongside the primary `state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateCandidate {
    /// Plugin-defined state name for the candidate.
    pub state: String,
    /// Candidate confidence from 0.0 to 1.0.
    #[serde(serialize_with = "serialize_confidence")]
    pub confidence: f32,
}

/// Serialize a confidence value with 3-decimal precision so JSON wire
/// output stays free of f32 representation noise.
///
/// The Lua plugin returns confidences like `0.62`, which lands in Rust as
/// an f32 that promotes to `0.6200000047683716` when serialized through
/// f64 — that's an artifact of the binary representation, not extra
/// precision, and it leaks straight to the JSON-RPC wire. Three decimals
/// is plenty for caller comparison; the classifier itself doesn't depend
/// on more precision.
pub(crate) fn serialize_confidence<S>(
    value: &f32,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let rounded = (f64::from(*value) * 1000.0).round() / 1000.0;
    serializer.serialize_f64(rounded)
}

/// Context handed to an [`Extension`]'s classifier on each call.
///
/// Lifetime-bound so the host does not need to allocate fresh `String` copies
/// on every classify call. The plugin is expected to clone or consume the
/// fields it needs.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClassifyContext<'a> {
    /// Full visible screen text. Kept for backward compatibility with any
    /// classifier path that wants the unsegmented view.
    pub screen: &'a str,
    /// Screen text with the bottom status-bar rows removed. Body-oriented
    /// classification should match against this to avoid false-positives
    /// from status strings.
    pub body_text: &'a str,
    /// Only the bottom status-bar rows. Available for plugins that want to
    /// inspect the status bar explicitly.
    pub status_text: &'a str,
    /// Transcript tail (host-managed retention) up to the current sequence.
    pub transcript: &'a str,
    /// Session sequence observed when this context was assembled.
    pub sequence: u64,
    /// Last intent the host applied through the extension, if any.
    ///
    /// Skipped from serialisation when `None` so the Lua side sees the field
    /// as absent (and therefore nil) rather than an `Option`-tagged value.
    /// mlua's serde adapter encodes `Option::None` as a tagged table when
    /// the field is present, which Lua's `or` operator treats as truthy and
    /// falls through to return the table rather than the fallback string.
    /// Skipping the field sidesteps the problem entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_intent: Option<&'a str>,
    /// How long the current screen has been stable, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_ms: Option<u64>,
    /// Threshold (in milliseconds) the host uses to treat a screen as "settled
    /// after a completed turn". Forwarded so plugins can reuse the same value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_turn_stable_ms: Option<u64>,
}

/// Action plan returned by extension intents (e.g. `send_prompt`, `approve`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ActionPlan {
    /// Ordered actions to apply to the session after the intent fires.
    pub actions: Vec<Action>,
    /// Optional intent name the host should remember for the next classify
    /// call (mirrors the prior `last_intent` field). Plugins that want the
    /// host to require an intent record should set this; otherwise leave it
    /// `None`.
    #[serde(default)]
    pub last_intent: Option<String>,
}

/// Runtime contract every plugin runtime implements.
///
/// [`LuaExtension`] is the only implementor shipped today; a WASM or external
/// process implementor would slot in here without changing the rest of the
/// core. The trait is intentionally not `Send`/`Sync`: the embedded
/// Implementors must be `Send` so adapter handles can live in shared
/// JSON-RPC state and be driven by whichever client connection happens
/// to be calling into the server. Only one thread will access a given
/// handle at a time — the shared registry serializes per-adapter access
/// behind a `Mutex<ExtensionEntry>`.
pub trait Extension: Send {
    /// Manifest describing this extension.
    fn manifest(&self) -> &PluginManifest;

    /// Classify the current screen / transcript state.
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Result<ExtensionStateSnapshot>;

    /// Build an action plan for a named intent (e.g. `"send_prompt"`).
    fn plan(&self, intent: &str, params: &Value) -> Result<ActionPlan>;

    /// Build a wait matcher for a named intent (e.g. `"wait_turn"`).
    fn wait_matcher(&self, intent: &str, params: &Value) -> Result<Matcher>;
}

/// Trusted Lua [`Extension`] implementation backed by a [`LuaPlugin`].
///
/// The plugin must export `classify` plus any intent functions and
/// `wait_*_matcher` functions it wants callers to be able to invoke through
/// [`ExtensionHandle::send`] / [`ExtensionHandle::wait`]. Exported function
/// names are caller-driven; the generic Extension contract does not bake in
/// any specific plugin's vocabulary.
pub struct LuaExtension {
    plugin: LuaPlugin,
    manifest: PluginManifest,
}

impl LuaExtension {
    /// Wrap a pre-loaded [`LuaPlugin`] and its [`PluginManifest`].
    #[must_use]
    pub fn new(plugin: LuaPlugin, manifest: PluginManifest) -> Self {
        Self { plugin, manifest }
    }

    /// Load the built-in Lua plugin with the given manifest name from the set
    /// of plugins bundled into the binary. Returns an error if the requested
    /// built-in is not known.
    ///
    /// Looks up the manifest + embedded source in
    /// [`BUILTIN_PLUGINS`](crate::plugin::BUILTIN_PLUGINS). Adding a new
    /// built-in is a one-line addition to that slice — no per-plugin Rust
    /// code is required here.
    pub fn built_in(name: &str) -> Result<Self> {
        let entry = BUILTIN_PLUGINS
            .iter()
            .find(|entry| (entry.manifest)().name == name)
            .ok_or_else(|| Error::Lua(format!("no built-in Lua extension named `{name}`")))?;
        let manifest = (entry.manifest)();
        let plugin = LuaPlugin::trusted_with_modules(&manifest, entry.source, entry.modules)?;
        Ok(Self::new(plugin, manifest))
    }

    /// Borrow the underlying [`LuaPlugin`]. Tests and downstream callers
    /// sometimes need to invoke plugin functions that aren't part of the
    /// [`Extension`] trait surface.
    #[must_use]
    pub fn plugin(&self) -> &LuaPlugin {
        &self.plugin
    }
}

impl Extension for LuaExtension {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn classify(&self, ctx: &ClassifyContext<'_>) -> Result<ExtensionStateSnapshot> {
        self.plugin.call("classify", ctx)
    }

    fn plan(&self, intent: &str, params: &Value) -> Result<ActionPlan> {
        self.plugin.call(intent, params)
    }

    fn wait_matcher(&self, intent: &str, params: &Value) -> Result<Matcher> {
        self.plugin.call(intent, params)
    }
}

/// Owns a PTY [`Session`] plus an [`Extension`] and orchestrates intents.
///
/// Mutating intents (those whose plan supplies `last_intent`) update the
/// recorded intent before the next classify call, so plugins can use the
/// transition for stable-state detection (e.g. "completed turn requires that
/// `last_intent == prompt_submitted`").
pub struct ExtensionHandle {
    session: Session,
    extension: Box<dyn Extension>,
    last_intent: Option<String>,
    completed_turn_stable_ms: u64,
}

impl ExtensionHandle {
    /// Build a new handle around an existing [`Session`] and [`Extension`].
    ///
    /// `completed_turn_stable_ms` is forwarded to the classifier as
    /// [`ClassifyContext::completed_turn_stable_ms`] so plugins can reuse the
    /// host's stability threshold.
    #[must_use]
    pub fn start(
        extension: Box<dyn Extension>,
        session: Session,
        completed_turn_stable_ms: u64,
    ) -> Self {
        Self {
            session,
            extension,
            last_intent: None,
            completed_turn_stable_ms,
        }
    }

    /// Access the underlying PTY session.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Borrow the wrapped extension.
    #[must_use]
    pub fn extension(&self) -> &dyn Extension {
        self.extension.as_ref()
    }

    /// Last intent the host applied through this handle, if any.
    #[must_use]
    pub fn last_intent(&self) -> Option<&str> {
        self.last_intent.as_deref()
    }

    /// Replace the recorded last intent. Callers wrapping an existing
    /// session may need to seed this on construction (e.g. to mark the
    /// session as "just spawned, classifier should see the starting state").
    pub fn set_last_intent(&mut self, intent: Option<String>) {
        self.last_intent = intent;
    }

    /// Classify current state. On classifier failure, returns a synthetic
    /// `plugin_error` state instead of erroring; use [`try_state`](Self::try_state)
    /// to surface plugin failures explicitly.
    #[must_use]
    pub fn state(&self) -> ExtensionStateSnapshot {
        self.try_state()
            .unwrap_or_else(|error| ExtensionStateSnapshot {
                state: "plugin_error".to_string(),
                confidence: 0.0,
                evidence: format!("plugin failed: {error}"),
                sequence: self.session.sequence(),
                candidates: Vec::new(),
                metadata: None,
            })
    }

    /// Classify current state, surfacing plugin failures.
    pub fn try_state(&self) -> Result<ExtensionStateSnapshot> {
        let snapshot = self.session.snapshot();
        let transcript = self.session.transcript();
        self.classify(&snapshot.plain_text, &transcript, snapshot.sequence, None)
    }

    /// Apply a named intent and return the post-apply state snapshot.
    ///
    /// If the plugin's action plan reports a `last_intent`, the handle records
    /// it before re-classifying. Plans without `last_intent` are treated as
    /// non-mutating (the recorded intent is left as-is), which is the
    /// conventional pattern for read-only or idempotent actions like
    /// approve/deny dialogs.
    pub fn send(&mut self, intent: &str, params: Value) -> Result<ExtensionStateSnapshot> {
        let params = ensure_params_object(params);
        let plan = self.extension.plan(intent, &params)?;
        self.apply_plan(&plan, intent)?;
        self.try_state()
    }

    /// Wait until the plugin's matcher for `intent` is satisfied or the
    /// timeout expires, then classify and return the resulting state alongside
    /// the structured outcome describing which matcher branch fired.
    ///
    /// The actual `stable_for` duration from the underlying
    /// [`MatchResult`](crate::matcher::MatchResult) is forwarded to the
    /// classifier as `stable_ms` so plugins see real screen stability rather
    /// than a configured threshold — this matters for adapters whose wait
    /// matcher does not include `screen_stable` and would otherwise classify
    /// against a stale assumption.
    pub fn wait(
        &self,
        intent: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(ExtensionStateSnapshot, Option<MatchOutcome>)> {
        let params = merge_wait_defaults(params, self.completed_turn_stable_ms);
        let matcher = self.extension.wait_matcher(intent, &params)?;
        let result = self.session.wait_for(&matcher, timeout)?;
        let stable_ms = u64::try_from(result.stable_for.as_millis()).unwrap_or(u64::MAX);
        let state = self.classify(
            &result.snapshot.plain_text,
            &result.transcript_tail,
            result.sequence,
            Some(stable_ms),
        )?;
        Ok((state, result.outcome))
    }

    /// Atomic [`send`](Self::send) followed by [`wait`](Self::wait) using a
    /// single mutex-held turn.
    ///
    /// `claudette` and similar consumers traditionally hand-rolled
    /// "submit prompt, then wait for the turn to complete" by chaining
    /// `adapter.send` and `adapter.wait`. In a multi-client setup another
    /// connection could slip a competing intent between those two calls.
    /// [`turn`](Self::turn) keeps both legs under the same `&mut self`
    /// borrow so this race is impossible at the type level.
    ///
    /// `wait_intent` defaults to the conventional `"wait_turn_matcher"` so
    /// simple call sites can pass `None`. The returned tuple mirrors
    /// [`wait`](Self::wait): the post-wait classifier state plus the
    /// matcher outcome that fired.
    pub fn turn(
        &mut self,
        send_intent: &str,
        send_params: Value,
        wait_intent: Option<&str>,
        wait_params: Value,
        timeout: Duration,
    ) -> Result<(ExtensionStateSnapshot, Option<MatchOutcome>)> {
        let _state_after_send = self.send(send_intent, send_params)?;
        let wait_intent = wait_intent.unwrap_or("wait_turn_matcher");
        self.wait(wait_intent, wait_params, timeout)
    }

    /// Apply an action plan, requiring that the plan supply `last_intent` and
    /// recording it as this handle's most recent intent. Use this for
    /// mutating intents that must update the classifier's intent tracking
    /// (e.g. `send_prompt`, `cancel`); fall back to [`send`](Self::send) for
    /// intents where the plan supplies `last_intent` opportunistically.
    pub fn apply_plan_with_required_intent(
        &mut self,
        plan: &ActionPlan,
        method: &str,
    ) -> Result<()> {
        self.apply_actions(&plan.actions)?;
        let intent = plan.last_intent.clone().ok_or_else(|| {
            Error::Lua(format!(
                "extension method `{method}` did not return last_intent"
            ))
        })?;
        // Mirror `apply_plan`'s empty-string semantics: an explicit
        // empty string is a CLEAR sentinel (used by no-op `send_prompt`
        // returns to drop a stale `prompt_submitted` intent). Recording
        // it literally as `Some("")` would leak truthiness into the
        // classifier and produce an invalid empty state.
        if intent.is_empty() {
            self.last_intent = None;
        } else {
            self.last_intent = Some(intent);
        }
        Ok(())
    }

    /// Apply a slice of actions to the underlying session in order.
    pub fn apply_actions(&self, actions: &[Action]) -> Result<()> {
        for action in actions {
            self.session.send(action.clone())?;
        }
        Ok(())
    }

    fn apply_plan(&mut self, plan: &ActionPlan, _intent: &str) -> Result<()> {
        self.apply_actions(&plan.actions)?;
        // Three states the plan's `last_intent` can express:
        //   * Some(non-empty)  → record this as the new intent.
        //   * Some("")         → explicit clear: drop whatever intent
        //                        is currently recorded. Used by no-op
        //                        plans (e.g. empty `send_prompt`) so
        //                        a stale `prompt_submitted` from a
        //                        prior submission doesn't keep the
        //                        classifier on the mid-turn branch.
        //   * None             → leave the recorded intent alone. The
        //                        conventional pattern for read-only
        //                        or idempotent actions (approve /
        //                        deny / dismiss_welcome / expand /
        //                        slash_command) that don't start a
        //                        new turn but also don't end one.
        if let Some(intent) = plan.last_intent.clone() {
            if intent.is_empty() {
                self.last_intent = None;
            } else {
                self.last_intent = Some(intent);
            }
        }
        Ok(())
    }

    fn classify(
        &self,
        screen: &str,
        transcript: &str,
        sequence: u64,
        stable_ms: Option<u64>,
    ) -> Result<ExtensionStateSnapshot> {
        let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
        let ctx = ClassifyContext {
            screen,
            body_text: &body_text,
            status_text: &status_text,
            transcript,
            sequence,
            last_intent: self.last_intent.as_deref(),
            stable_ms,
            completed_turn_stable_ms: Some(self.completed_turn_stable_ms),
        };
        self.extension.classify(&ctx)
    }
}

/// Coerce intent params into a JSON object so plugin handlers can index
/// into them without crashing the runtime.
///
/// Generic callers of `adapter.send` / `adapter.wait` typically omit the
/// nested `params` field entirely, which deserialises to `Value::Null`.
/// Forwarding that straight to a Lua plugin makes the plugin index a
/// userdata-Null sentinel (mlua serde representation of a JSON null) and
/// blow up at runtime. Promote Null to an empty object so the plugin
/// always receives a table. Scalars and arrays are handed back verbatim
/// for tests and future plugins that take non-object params.
fn ensure_params_object(params: Value) -> Value {
    match params {
        Value::Null => Value::Object(serde_json::Map::new()),
        other => other,
    }
}

/// Like [`ensure_params_object`] but also injects the host's configured
/// `completed_turn_stable_ms` if the caller did not already supply one.
/// Used by [`ExtensionHandle::wait`] so generic callers don't need to
/// know about plugin-side stability thresholds.
fn merge_wait_defaults(params: Value, completed_turn_stable_ms: u64) -> Value {
    let coerced = ensure_params_object(params);
    let Value::Object(mut object) = coerced else {
        return coerced;
    };
    object
        .entry("completed_turn_stable_ms")
        .or_insert_with(|| Value::from(completed_turn_stable_ms));
    Value::Object(object)
}

/// The bottom `status_rows` lines are treated as the status bar. Short
/// screens (where `lines.len() <= status_rows * 2`) are returned as
/// body-only because they don't have a meaningful status bar to peel off.
#[must_use]
pub fn split_status_bar(screen: &str, status_rows: usize) -> (String, String) {
    // `split_terminator` does not emit a trailing empty element when `screen`
    // ends in `\n`. `split('\n')` would, which shifts the cutoff up by one
    // and lets the topmost status row leak into `body` on any fixture that
    // ends with a newline (which most of them do). The fixture-driven
    // classifier matrix would still pass under either split today, but the
    // body/status partition is meant to mirror the rendered rows; if a future
    // status string contains a keyword the classifier matches on, the leak
    // would re-introduce the false-positives this split exists to prevent.
    let lines: Vec<&str> = screen.split_terminator('\n').collect();
    if lines.is_empty() {
        return (String::new(), String::new());
    }
    // Only split a screen that's tall enough to actually have a body + status
    // bar. The status-bar pattern (separator + status rows at the bottom)
    // only manifests on full-height TUI screens. Short fixtures and small
    // windows are entirely body — splitting them would shove the only content
    // into status_text and break classification.
    if lines.len() <= status_rows * 2 {
        return (screen.to_string(), String::new());
    }
    let cutoff = lines.len() - status_rows;
    let body = lines[..cutoff].join("\n");
    let status = lines[cutoff..].join("\n");
    (body, status)
}

/// Apply the body/status split that the classifier uses, for diagnostic RPC
/// methods like `adapter.inspect` that want to surface what the classifier
/// would have seen.
#[must_use]
pub fn split_status_bar_for_inspect(screen: &str) -> (String, String) {
    split_status_bar(screen, STATUS_BAR_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_status_bar_partitions_screen_into_body_and_status() {
        let screen =
            "Claude Code v2.1.142\nHaiku 4.5\n\n────\n❯  \n────\nstatus line one\nstatus line two";
        let (body, status) = split_status_bar(screen, 3);
        assert!(body.contains("Claude Code v2.1.142"));
        assert!(body.contains("❯  "));
        assert!(!body.contains("status line one"));
        assert!(!body.contains("status line two"));
        assert!(status.contains("status line one"));
        assert!(status.contains("status line two"));
    }

    #[test]
    fn split_status_bar_ignores_trailing_newline() {
        // Regression: a screen that ends with `\n` would split into one extra
        // (empty) element under `split('\n')`, pushing the cutoff up and
        // leaking the top status row into body. With `split_terminator` the
        // partition is independent of whether the screen carries a trailing
        // newline.
        let nine_lines = "L0\nL1\nL2\nL3\nL4\nL5\nL6\nL7\nL8";
        let nine_lines_with_nl = format!("{nine_lines}\n");
        let (body_a, status_a) = split_status_bar(nine_lines, 3);
        let (body_b, status_b) = split_status_bar(&nine_lines_with_nl, 3);
        assert_eq!(body_a, body_b, "trailing newline must not shift body");
        assert_eq!(status_a, status_b, "trailing newline must not shift status");
        assert!(body_a.contains("L5"), "body should end at L5");
        assert!(!body_a.contains("L6"), "L6 belongs to status");
        assert!(status_a.contains("L6"));
        assert!(status_a.contains("L8"));
    }

    #[test]
    fn split_status_bar_handles_short_screens_without_panic() {
        assert_eq!(split_status_bar("", 3), (String::new(), String::new()));
        assert_eq!(
            split_status_bar("only line", 3),
            ("only line".to_string(), String::new()),
        );
        let (body, status) = split_status_bar("a\nb", 3);
        assert_eq!(body, "a\nb");
        assert_eq!(status, "");
    }

    #[test]
    fn built_in_claude_code_extension_loads() {
        let extension = LuaExtension::built_in("claude-code").expect("built-in claude-code");
        assert_eq!(extension.manifest().name, "claude-code");
    }

    #[test]
    fn built_in_rejects_unknown_name() {
        match LuaExtension::built_in("not-a-plugin") {
            Ok(_) => panic!("unknown built-in should not load"),
            Err(error) => assert!(matches!(error, Error::Lua(_))),
        }
    }

    #[test]
    fn plugin_accessor_exposes_underlying_lua_plugin() {
        // Downstream callers (and integration tests) need access to the
        // wrapped LuaPlugin for permission introspection and direct
        // function calls that aren't part of the Extension trait surface.
        let extension = LuaExtension::built_in("claude-code").expect("built-in claude-code");
        assert!(
            extension
                .plugin()
                .has_permission(&crate::plugin::PluginPermission::SessionSpawn),
            "claude-code manifest declares session.spawn",
        );
    }

    /// Regression: serialising a [`ClassifyContext`] with `last_intent = None`
    /// through mlua's serde adapter once produced a tagged Option value on the
    /// Lua side. The Lua classifier's `or` fallback (`input.last_intent or
    /// "starting"`) then short-circuited to that truthy table instead of the
    /// fallback string, the host's deserialiser saw a non-string `state`
    /// field, and the whole call returned `plugin_error`. The fix is to skip
    /// `Option::None` fields when serialising the context; the Lua side reads
    /// missing fields as nil and the fallback works as intended.
    #[test]
    fn classify_context_serialises_with_omitted_none_fields() {
        let ctx = ClassifyContext {
            screen: "",
            body_text: "",
            status_text: "",
            transcript: "",
            sequence: 0,
            last_intent: None,
            stable_ms: None,
            completed_turn_stable_ms: None,
        };
        let value = serde_json::to_value(ctx).expect("serialise ClassifyContext");
        let object = value
            .as_object()
            .expect("ClassifyContext serialises to an object");
        assert!(
            !object.contains_key("last_intent"),
            "None last_intent must be omitted; got {value}"
        );
        assert!(
            !object.contains_key("stable_ms"),
            "None stable_ms must be omitted; got {value}"
        );
        assert!(
            !object.contains_key("completed_turn_stable_ms"),
            "None completed_turn_stable_ms must be omitted; got {value}"
        );
    }

    /// Regression: end-to-end version of the test above. Calling
    /// `ExtensionHandle::state()` against a freshly-spawned session (empty
    /// screen + `last_intent = None`) used to fall through the host call,
    /// fail to deserialise, and return the `plugin_error` fallback. The
    /// classifier should now succeed and report `starting`.
    #[test]
    #[cfg(unix)]
    fn handle_state_with_empty_screen_classifies_as_starting_not_plugin_error()
    -> std::result::Result<(), Error> {
        // /bin/sh -lc "sleep 30" gives us a session that exists but never
        // writes anything, so the classifier sees an empty screen.
        let target = crate::target::Target::new("/bin/sh").args(["-lc", "sleep 30"]);
        let session = Session::spawn(crate::session::SessionConfig::new(target))?;
        let extension = LuaExtension::built_in("claude-code")?;
        let handle = ExtensionHandle::start(Box::new(extension), session, 300);
        let state = handle.state();
        assert_eq!(state.state, "starting", "evidence: {}", state.evidence);
        assert!(
            !state.evidence.starts_with("plugin failed"),
            "plugin should not error on empty screen: {}",
            state.evidence
        );
        Ok(())
    }

    #[test]
    fn merge_wait_defaults_coerces_null_and_injects_stable_ms() {
        // Generic adapter.wait callers omit nested `params`, which serde
        // deserialises as Value::Null. Forwarding Null to a Lua matcher
        // function makes mlua's serde adapter produce a userdata sentinel
        // that the plugin cannot index, so the host must coerce it to an
        // object before calling the plugin.
        let merged = merge_wait_defaults(Value::Null, 300);
        assert_eq!(merged, serde_json::json!({"completed_turn_stable_ms": 300}));
    }

    #[test]
    fn merge_wait_defaults_preserves_caller_supplied_stable_ms() {
        // The host injects its configured threshold only when the caller
        // did not already supply one. Callers that want a different
        // stability window must still be able to override.
        let merged = merge_wait_defaults(serde_json::json!({"completed_turn_stable_ms": 50}), 300);
        assert_eq!(merged, serde_json::json!({"completed_turn_stable_ms": 50}));
    }

    #[test]
    fn confidence_serializes_with_three_decimal_precision() {
        // `0.62_f32` promotes to `0.6200000047683716_f64`, which used to
        // land on the JSON-RPC wire as-is and looked like noise to
        // callers. The custom serializer rounds to 3 decimals.
        let snapshot = ExtensionStateSnapshot {
            state: "thinking".into(),
            confidence: 0.62,
            evidence: "test".into(),
            sequence: 0,
            candidates: Vec::new(),
            metadata: None,
        };
        let wire = serde_json::to_string(&snapshot).expect("serialize");
        assert!(
            wire.contains("\"confidence\":0.62"),
            "expected rounded confidence on the wire; got: {wire}",
        );
        assert!(
            !wire.contains("0.62000000"),
            "f32 noise leaked through: {wire}",
        );
    }

    #[test]
    fn metadata_field_is_omitted_from_wire_when_none() {
        // The `metadata` field is opt-in plugin sugar; when a classifier
        // doesn't attach anything the JSON-RPC wire must NOT carry a
        // `"metadata": null` key. Catches a future change that flips the
        // skip_serializing_if attribute or drops the Option wrapper.
        let snapshot = ExtensionStateSnapshot::new("ready", 7);
        let wire = serde_json::to_string(&snapshot).expect("serialize");
        assert!(
            !wire.contains("metadata"),
            "`metadata: None` must not appear on the wire; got: {wire}",
        );

        let mut with_metadata = snapshot.clone();
        with_metadata.metadata = Some(serde_json::json!({"usage": {"cost_usd": 0.12}}));
        let wire = serde_json::to_string(&with_metadata).expect("serialize");
        assert!(
            wire.contains("\"metadata\":{\"usage\":{\"cost_usd\":0.12}}"),
            "metadata must serialize verbatim when Some; got: {wire}",
        );
    }

    #[test]
    fn candidate_confidence_also_rounded_on_wire() {
        // Same fix applies to runner-up candidates so the wire shape is
        // consistent whether the caller reads `state` or `candidates`.
        let candidate = StateCandidate {
            state: "thinking".into(),
            confidence: 0.86,
        };
        let wire = serde_json::to_string(&candidate).expect("serialize");
        assert!(
            wire.contains("\"confidence\":0.86"),
            "expected rounded candidate confidence; got: {wire}",
        );
    }

    #[test]
    fn ensure_params_object_promotes_null_to_empty_object() {
        // mlua's serde adapter renders JSON null as a userdata sentinel
        // that Lua handlers cannot index, so the host must promote Null
        // to an empty object before calling any plugin function. Generic
        // `adapter.send` callers that omit `params` rely on this.
        assert_eq!(ensure_params_object(Value::Null), serde_json::json!({}));
    }

    #[test]
    fn ensure_params_object_leaves_existing_objects_untouched() {
        let v = serde_json::json!({"prompt": "hello"});
        assert_eq!(ensure_params_object(v.clone()), v);
    }

    #[test]
    fn ensure_params_object_passes_scalars_through() {
        // Future plugins may legitimately accept non-object params (a
        // single string, a number, …). The coercion only fixes Null; it
        // does not force a particular shape on the caller.
        assert_eq!(
            ensure_params_object(Value::from("verbatim")),
            Value::from("verbatim"),
        );
    }

    #[test]
    fn merge_wait_defaults_keeps_other_object_keys() {
        // Future plugins may take additional params alongside the stability
        // hint. The merge must only add the missing key, not rewrite the
        // whole object.
        let merged = merge_wait_defaults(serde_json::json!({"pattern": "ready"}), 300);
        assert_eq!(
            merged,
            serde_json::json!({"pattern": "ready", "completed_turn_stable_ms": 300}),
        );
    }
}
