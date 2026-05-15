//! Generic extension layer that lets adapters plug into ptywright through a
//! small, runtime-agnostic trait.
//!
//! The [`Extension`] trait abstracts whatever produces classifier output and
//! action plans for an interactive TUI. The Claude Code adapter is the first
//! consumer ([`LuaExtension`] is the only implementor shipped today), but a
//! future WASM or external-process plugin can drop in by implementing this
//! trait without changing the rest of the core.
//!
//! Everything here is intentionally application-agnostic: no Claude-specific
//! state names, no Claude-specific intents. The adapter shim in
//! `src/adapters/claude_code.rs` translates between this generic surface and
//! its public [`ClaudeCodeState`](crate::ClaudeCodeState) enum.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::Action;
use crate::error::{Error, Result};
use crate::lua_plugin::LuaPlugin;
use crate::matcher::Matcher;
use crate::plugin::{PluginManifest, claude_code_manifest};
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
/// have to know any specific adapter's vocabulary. Confidence, evidence, and
/// sequence follow the Milestone 16 convention from the Claude Code adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionStateSnapshot {
    /// Plugin-defined classification, e.g. `"ready"`, `"thinking"`, etc.
    pub state: String,
    /// Confidence from 0.0 to 1.0.
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
}

/// Runner-up classification produced alongside the primary `state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateCandidate {
    /// Plugin-defined state name for the candidate.
    pub state: String,
    /// Candidate confidence from 0.0 to 1.0.
    pub confidence: f32,
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
///
/// Generic version of the per-adapter `ActionPlan` types that previously
/// lived in `src/adapters/claude_code.rs`.
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
/// [`LuaPlugin`] uses `Rc` internally, so each handle is owned by a single
/// thread. Multi-threaded RPC transports already serialize per-connection
/// access via separate [`ExtensionHandle`] instances.
pub trait Extension {
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
/// The plugin must export `classify`, the intent functions named by the
/// adapter shim, and matcher constructors named by the adapter shim (e.g.
/// `wait_turn_matcher`). Exported function names are caller-driven so the
/// generic Extension contract does not bake in any specific adapter's
/// vocabulary.
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
    pub fn built_in(name: &str) -> Result<Self> {
        match name {
            "claude-code" => {
                let manifest = claude_code_manifest();
                let plugin =
                    LuaPlugin::trusted(&manifest, include_str!("../plugins/claude-code/main.lua"))?;
                Ok(Self::new(plugin, manifest))
            }
            other => Err(Error::Lua(format!(
                "no built-in Lua extension named `{other}`"
            ))),
        }
    }

    /// Borrow the underlying [`LuaPlugin`]. Adapter shims sometimes need to
    /// call functions that aren't part of the [`Extension`] trait surface
    /// during tests.
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
/// This is the generic version of the per-adapter "session + plugin" handle
/// that previously lived in `src/adapters/claude_code.rs`. The same
/// state-after-apply semantics from Milestone 21.6 apply: mutating intents
/// (those whose plan supplies `last_intent`) update the recorded intent
/// before the next classify call.
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

    /// Last intent the host applied through this handle, if any. Adapter shims
    /// use this to seed their own typed `last_intent` field.
    #[must_use]
    pub fn last_intent(&self) -> Option<&str> {
        self.last_intent.as_deref()
    }

    /// Replace the recorded last intent. Adapter shims that wrap an existing
    /// session may need to seed this on construction.
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
    /// non-mutating (the recorded intent is left as-is) and behave like
    /// `approve`/`deny` in the Claude Code adapter.
    pub fn send(&mut self, intent: &str, params: Value) -> Result<ExtensionStateSnapshot> {
        let plan = self.extension.plan(intent, &params)?;
        self.apply_plan(&plan, intent)?;
        self.try_state()
    }

    /// Wait until the plugin's matcher for `intent` is satisfied or the
    /// timeout expires, then classify and return the resulting state.
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
    ) -> Result<ExtensionStateSnapshot> {
        let matcher = self.extension.wait_matcher(intent, &params)?;
        let result = self.session.wait_for(&matcher, timeout)?;
        let stable_ms = u64::try_from(result.stable_for.as_millis()).unwrap_or(u64::MAX);
        self.classify(
            &result.snapshot.plain_text,
            &result.transcript_tail,
            result.sequence,
            Some(stable_ms),
        )
    }

    /// Apply an action plan, requiring that the plan supply `last_intent` and
    /// recording it as this handle's most recent intent. Intended for the
    /// adapter shim's mutating intents (e.g. `send_prompt`, `cancel`).
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
        self.last_intent = Some(intent);
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
        if let Some(intent) = plan.last_intent.clone() {
            self.last_intent = Some(intent);
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

/// Split a rendered screen into `(body, status)` halves.
///
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
/// methods like `claude.inspect` that want to surface what the classifier
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
}
