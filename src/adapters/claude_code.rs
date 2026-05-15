use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

const COMPLETED_TURN_STABLE_MS: u64 = 300;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::extension::{ExtensionHandle, ExtensionStateSnapshot, LuaExtension};
use crate::session::{Session, SessionConfig};
use crate::target::{Target, TerminalSize};

/// Configuration for starting interactive Claude Code in a PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCodeConfig {
    /// Claude executable name or path. Defaults to `claude`.
    pub program: String,
    /// Extra interactive CLI arguments. Do not use `-p` here.
    pub args: Vec<String>,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Environment overrides for the child process.
    pub env: BTreeMap<String, String>,
    /// Initial terminal size.
    pub size: TerminalSize,
}

impl ClaudeCodeConfig {
    /// Convert this adapter configuration to a generic ptywright target.
    #[must_use]
    pub fn target(&self) -> Target {
        let mut target = Target::new(self.program.clone())
            .args(self.args.clone())
            .size(self.size);
        target.cwd = self.cwd.clone();
        target.env = self.env.clone();
        target
    }
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            program: "claude".to_string(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            size: TerminalSize::new(40, 120),
        }
    }
}

/// Coarse Claude Code TUI state inferred from screen and transcript evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeCodeState {
    /// Process has been spawned but no useful screen evidence is available yet.
    Starting,
    /// TUI appears ready for user input.
    Ready,
    /// A prompt was submitted by this adapter.
    PromptSubmitted,
    /// Claude appears to be working on a turn.
    Thinking,
    /// Claude appears to be waiting for tool/permission approval.
    WaitingForPermission,
    /// Claude appears to be waiting for plan approval.
    WaitingForPlanApproval,
    /// Claude appears to be waiting for workspace-trust confirmation.
    ///
    /// Distinct from `WaitingForPermission` because the trust dialog uses a
    /// numbered list (`1 = Yes, proceed` / `2 = No, exit`) rather than the
    /// Bash/Edit-style "press Enter to approve" UI. Approving via Enter
    /// alone does not accept option 1, so the Lua plugin exposes a
    /// separate `approve_trust` / `deny_trust` intent that types the
    /// numeric option first.
    WaitingForTrust,
    /// Claude appears to be waiting for ordinary user input.
    WaitingForUserInput,
    /// A turn appears complete.
    CompletedTurn,
    /// Cancellation was requested.
    Cancelling,
    /// Process exited.
    Exited,
    /// State could not be classified.
    Error,
    /// The built-in Lua adapter failed before Claude Code state could be read.
    PluginError,
}

/// State classification with evidence and confidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeCodeStateSnapshot {
    /// Inferred state.
    pub state: ClaudeCodeState,
    /// Confidence from 0.0 to 1.0.
    #[serde(serialize_with = "crate::extension::serialize_confidence")]
    pub confidence: f32,
    /// Human-readable evidence used for the classification.
    pub evidence: String,
    /// Session sequence observed for the classification.
    pub sequence: u64,
}

impl From<ExtensionStateSnapshot> for ClaudeCodeStateSnapshot {
    fn from(value: ExtensionStateSnapshot) -> Self {
        let ExtensionStateSnapshot {
            state,
            confidence,
            evidence,
            sequence,
            candidates: _,
        } = value;
        let (state_enum, evidence) = match state_from_name(&state) {
            Some(parsed) => (parsed, evidence),
            None => (
                ClaudeCodeState::Error,
                if evidence.is_empty() {
                    format!("unknown extension state `{state}`")
                } else {
                    format!("unknown extension state `{state}`: {evidence}")
                },
            ),
        };
        Self {
            state: state_enum,
            confidence,
            evidence,
            sequence,
        }
    }
}

/// Interactive Claude Code adapter backed by a generic ptywright session.
pub struct ClaudeCodeAdapter {
    handle: ExtensionHandle,
}

impl ClaudeCodeAdapter {
    /// Spawn interactive Claude Code in a PTY.
    pub fn start(config: ClaudeCodeConfig) -> Result<Self> {
        let session = Session::spawn(SessionConfig::new(config.target()))?;
        Self::from_session_with_starting_intent(session)
    }

    /// Wrap an existing session. Useful for tests or externally managed sessions.
    pub fn from_session(session: Session) -> Result<Self> {
        let extension = LuaExtension::built_in("claude-code")?;
        let handle = ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);
        Ok(Self { handle })
    }

    fn from_session_with_starting_intent(session: Session) -> Result<Self> {
        let extension = LuaExtension::built_in("claude-code")?;
        let mut handle =
            ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);
        handle.set_last_intent(Some(state_name(ClaudeCodeState::Starting)?));
        Ok(Self { handle })
    }

    /// Access the underlying generic session.
    #[must_use]
    pub fn session(&self) -> &Session {
        self.handle.session()
    }

    /// Classify current Claude Code state from visible screen and transcript evidence.
    #[must_use]
    pub fn state(&self) -> ClaudeCodeStateSnapshot {
        self.try_state()
            .unwrap_or_else(|error| ClaudeCodeStateSnapshot {
                state: ClaudeCodeState::PluginError,
                confidence: 0.0,
                evidence: format!("Claude Code Lua plugin failed: {error}"),
                sequence: self.handle.session().sequence(),
            })
    }

    /// Classify current Claude Code state and surface Lua plugin failures.
    pub fn try_state(&self) -> Result<ClaudeCodeStateSnapshot> {
        Ok(self.handle.try_state()?.into())
    }

    /// Send a prompt to the interactive Claude Code TUI.
    pub fn send_prompt(&mut self, prompt: impl AsRef<str>) -> Result<ClaudeCodeStateSnapshot> {
        let plan = self.handle.extension().plan(
            "send_prompt",
            &serde_json::json!({ "prompt": prompt.as_ref() }),
        )?;
        self.handle
            .apply_plan_with_required_intent(&plan, "send_prompt")?;
        self.try_state()
    }

    /// Wait until Claude appears to need user input, approval, or has completed a turn.
    pub fn wait_turn(&self, timeout: Duration) -> Result<ClaudeCodeStateSnapshot> {
        Ok(self
            .handle
            .wait(
                "wait_turn_matcher",
                serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
                timeout,
            )?
            .into())
    }

    /// Approve the current Claude Code prompt using the Lua adapter's action plan.
    ///
    /// Returns the post-apply state snapshot so callers don't need to round-trip
    /// a separate `claude.state` query after approving. The snapshot is the
    /// classifier's read at the moment after the approve action ran; the screen
    /// has not necessarily settled, so callers that want a stable
    /// classification should follow up with `wait_turn`.
    pub fn approve(&self) -> Result<ClaudeCodeStateSnapshot> {
        let plan = self
            .handle
            .extension()
            .plan("approve", &serde_json::json!({}))?;
        self.handle.apply_actions(&plan.actions)?;
        self.try_state()
    }

    /// Deny the current Claude Code prompt using the Lua adapter's action plan.
    ///
    /// Returns the post-apply state snapshot. See [`approve`](Self::approve)
    /// for the stability caveat.
    pub fn deny(&self) -> Result<ClaudeCodeStateSnapshot> {
        let plan = self
            .handle
            .extension()
            .plan("deny", &serde_json::json!({}))?;
        self.handle.apply_actions(&plan.actions)?;
        self.try_state()
    }

    /// Cancel the current turn with the Lua adapter's action plan.
    pub fn cancel(&mut self) -> Result<ClaudeCodeStateSnapshot> {
        let plan = self
            .handle
            .extension()
            .plan("cancel", &serde_json::json!({}))?;
        self.handle
            .apply_plan_with_required_intent(&plan, "cancel")?;
        self.try_state()
    }
}

fn state_name(state: ClaudeCodeState) -> Result<String> {
    serde_json::to_value(state)?
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| Error::Lua("Claude Code state did not serialize to a string".into()))
}

fn state_from_name(name: &str) -> Option<ClaudeCodeState> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::Action;
    use crate::extension::{ActionPlan, LuaExtension, StateCandidate};
    use crate::matcher::Matcher;
    use crate::screen::CursorState;
    use crate::screen::ScreenSnapshot;
    use crate::target::TerminalSize;

    fn classify_state(
        extension: &LuaExtension,
        screen: &str,
        transcript: &str,
        sequence: u64,
        last_intent: Option<ClaudeCodeState>,
        stable_ms: Option<u64>,
    ) -> Result<ClaudeCodeStateSnapshot> {
        use crate::extension::{ClassifyContext, Extension, STATUS_BAR_ROWS, split_status_bar};
        let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
        let intent_name = last_intent.map(state_name).transpose()?;
        let ctx = ClassifyContext {
            screen,
            body_text: &body_text,
            status_text: &status_text,
            transcript,
            sequence,
            last_intent: intent_name.as_deref(),
            stable_ms,
            completed_turn_stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        };
        Ok(extension.classify(&ctx)?.into())
    }

    fn claude_plugin() -> Result<LuaExtension> {
        LuaExtension::built_in("claude-code")
    }

    fn lua_call_plan(
        extension: &LuaExtension,
        intent: &str,
        params: serde_json::Value,
    ) -> ActionPlan {
        use crate::extension::Extension;
        extension.plan(intent, &params).expect("plan from Lua")
    }

    fn lua_call_matcher(
        extension: &LuaExtension,
        intent: &str,
        params: serde_json::Value,
    ) -> Matcher {
        use crate::extension::Extension;
        extension
            .wait_matcher(intent, &params)
            .expect("matcher from Lua")
    }

    fn parse_last_intent(plan: &ActionPlan) -> Option<ClaudeCodeState> {
        plan.last_intent.as_deref().and_then(state_from_name)
    }

    #[test]
    fn default_config_targets_interactive_claude_without_print_mode() {
        let target = ClaudeCodeConfig::default().target();

        assert_eq!(target.program, "claude");
        assert!(
            !target
                .args
                .iter()
                .any(|arg| arg == "-p" || arg == "--print")
        );
        assert_eq!(target.size, TerminalSize::new(40, 120));
    }

    #[test]
    #[cfg(unix)]
    fn adapter_from_session_executes_lua_action_plans() -> Result<()> {
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "cat"]))?;
        let mut adapter = ClaudeCodeAdapter::from_session(session)?;

        let _ = adapter.send_prompt("hello from lua")?;
        adapter.session().wait_for(
            &Matcher::TranscriptContains("hello from lua".to_string()),
            Duration::from_secs(2),
        )?;
        adapter.approve()?;
        adapter.deny()?;
        let _ = adapter.cancel()?;
        let _ = adapter.session().kill();

        Ok(())
    }

    #[test]
    fn lua_plugin_supplies_prompt_action_plan() {
        // Claude Code v2.1+ enables bracketed paste, so the plan must use
        // the bracketed variant to keep the trailing Enter from being
        // absorbed into the paste tokeniser on longer prompts. The plain
        // `Action::Paste` variant still exists for callers driving
        // programs that have not enabled bracketed paste — see the
        // Codex review note that prompted this split.
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(
            &extension,
            "send_prompt",
            serde_json::json!({ "prompt": "hello Claude" }),
        );

        assert_eq!(
            plan.actions,
            vec![
                Action::BracketedPaste("hello Claude".to_string()),
                Action::Key(crate::Key::Enter),
            ]
        );
        assert_eq!(
            parse_last_intent(&plan),
            Some(ClaudeCodeState::PromptSubmitted)
        );
    }

    #[test]
    fn lua_plugin_supplies_turn_wait_matcher() {
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let matcher = lua_call_matcher(
            &extension,
            "wait_turn_matcher",
            serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
        );

        let Matcher::All(matchers) = matcher else {
            panic!("expected Lua wait matcher to require all conditions");
        };
        let Some(Matcher::Any(boundary_matchers)) = matchers
            .iter()
            .find(|matcher| matches!(matcher, Matcher::Any(_)))
        else {
            panic!("expected Lua wait matcher to include boundary alternatives");
        };
        assert!(
            boundary_matchers
                .iter()
                .any(|matcher| matcher == &Matcher::ContainsText("Total cost:".to_string()))
        );
        // The classifier recognises Claude Code 2.1's `Accessing workspace`
        // / `Yes, I trust this folder` dialog; the wait matcher must wake
        // on the same dialog or callers waiting after `adapter.start`
        // against a fresh untrusted directory will time out. Lock the
        // anchor in here so a future Lua edit can't silently drop it.
        assert!(
            boundary_matchers
                .iter()
                .any(|matcher| matcher
                    == &Matcher::ContainsText("Accessing workspace".to_string())),
            "wait matcher missing v2 trust dialog anchor; boundary matchers were {boundary_matchers:?}",
        );
        assert!(
            boundary_matchers
                .iter()
                .any(|matcher| matcher
                    == &Matcher::ContainsText("Yes, I trust this folder".to_string())),
            "wait matcher missing v2 trust confirmation anchor; boundary matchers were {boundary_matchers:?}",
        );
        assert!(matchers.iter().any(|matcher| matches!(
            matcher,
            Matcher::ScreenStable {
                min_ms: COMPLETED_TURN_STABLE_MS
            }
        )));
    }

    #[test]
    fn lua_turn_wait_matcher_matches_prompt_line() {
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let matcher = lua_call_matcher(
            &extension,
            "wait_turn_matcher",
            serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
        );
        let snapshot = ScreenSnapshot {
            size: TerminalSize::new(3, 20),
            cursor: CursorState {
                row: 1,
                col: 2,
                visible: true,
            },
            sequence: 1,
            plain_text: "work complete\n > \r\n".to_string(),
            cells: Vec::new(),
            alternate_screen: false,
            application_cursor: false,
            application_keypad: false,
            title: None,
        };

        assert!(matcher.is_match_with_context(
            &snapshot,
            "",
            crate::matcher::MatcherContext {
                stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
                process_exited: false,
            },
        ));
    }

    #[test]
    fn classifier_detects_permission_prompt() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "Do you want to proceed? Allow tool use",
            "",
            3,
            None,
            None,
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::WaitingForPermission);
        assert!(state.confidence > 0.8);
    }

    #[test]
    fn classifier_detects_plan_approval() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "Plan ready. Approve plan to proceed",
            "",
            4,
            None,
            None,
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::WaitingForPlanApproval);
    }

    #[test]
    fn classifier_detects_thinking() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "Thinking... Esc to interrupt",
            "",
            5,
            None,
            None,
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::Thinking);
    }

    #[test]
    fn classifier_detects_completed_turn_after_prompt_submission() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "work completed\n>",
            "",
            6,
            Some(ClaudeCodeState::PromptSubmitted),
            Some(COMPLETED_TURN_STABLE_MS),
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::CompletedTurn);
        assert_eq!(
            state.evidence,
            "stable input prompt after prompt submission"
        );
    }

    #[test]
    fn classifier_detects_usage_screen_as_completed_turn() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            include_str!("../../tests/fixtures/claude_code/usage.txt"),
            "",
            8,
            Some(ClaudeCodeState::PromptSubmitted),
            Some(COMPLETED_TURN_STABLE_MS),
        )
        .expect("classify usage fixture via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::CompletedTurn);
        assert_eq!(state.evidence, "stable usage screen detected");
    }

    #[test]
    fn classifier_requires_stable_prompt_for_completed_turn() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "work completed\n>",
            "",
            6,
            Some(ClaudeCodeState::PromptSubmitted),
            None,
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::WaitingForUserInput);
        assert_eq!(state.evidence, "input prompt glyph detected");
    }

    #[test]
    fn classifier_prefers_active_work_over_prompt_glyph() {
        let state = classify_state(
            &claude_plugin().expect("load plugin"),
            "Thinking... Esc to interrupt\n>",
            "",
            7,
            Some(ClaudeCodeState::PromptSubmitted),
            Some(COMPLETED_TURN_STABLE_MS),
        )
        .expect("classify via Lua plugin");

        assert_eq!(state.state, ClaudeCodeState::Thinking);
        assert_eq!(state.evidence, "active work indicator detected");
    }

    #[test]
    fn lua_cancel_sets_cancelling_intent() {
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(&extension, "cancel", serde_json::json!({}));

        assert_eq!(plan.actions, vec![Action::Interrupt]);
        assert_eq!(parse_last_intent(&plan), Some(ClaudeCodeState::Cancelling));
    }

    #[test]
    fn classifier_reports_cancelling_while_screen_is_still_settling() {
        // Right after `cancel` is sent the PTY needs a moment to render the
        // post-interrupt state. If the classifier just falls through to its
        // ordinary branches during that window it reports
        // `waiting_for_user_input`, leaving callers with no signal that the
        // cancel actually landed. Hold `cancelling` until the screen has
        // been stable for `completed_turn_stable_ms` so polling drivers can
        // observe the transition.
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let mid_cancel_screen = "❯ count to 10\n\n(interrupting…)\n\n────────\n❯\n";
        let state = classify_state(
            &extension,
            mid_cancel_screen,
            "",
            42,
            Some(ClaudeCodeState::Cancelling),
            Some(50),
        )
        .expect("classify mid-cancel screen");
        assert_eq!(state.state, ClaudeCodeState::Cancelling);
        assert!(
            state.evidence.contains("cancel intent recently applied"),
            "evidence should mention recent cancel: {}",
            state.evidence,
        );
    }

    #[test]
    fn classifier_releases_cancelling_once_screen_settles() {
        // Once the screen has been stable for the configured window the
        // classifier should fall through to whatever the post-cancel screen
        // actually shows. For an idle prompt that means
        // `waiting_for_user_input`; this lets the polling driver see cancel
        // → cancelling → waiting_for_user_input as a clean sequence rather
        // than getting stuck reporting `cancelling` forever.
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let post_cancel_idle = "❯ count to 10\n\n────────\n❯\n";
        let state = classify_state(
            &extension,
            post_cancel_idle,
            "",
            43,
            Some(ClaudeCodeState::Cancelling),
            Some(COMPLETED_TURN_STABLE_MS + 100),
        )
        .expect("classify settled post-cancel screen");
        assert_ne!(
            state.state,
            ClaudeCodeState::Cancelling,
            "cancelling must release once the screen settles; got evidence {}",
            state.evidence,
        );
    }

    #[test]
    fn lua_approve_trust_types_numeric_option_one() {
        // The trust dialog requires typing "1" before Enter; a bare Enter
        // does not accept option 1 in the Claude Code TUI. Lock the action
        // sequence down so a future Lua edit can't silently regress it.
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(&extension, "approve_trust", serde_json::json!({}));

        assert_eq!(
            plan.actions,
            vec![
                Action::Text("1".to_string()),
                Action::Key(crate::action::Key::Enter),
            ],
        );
    }

    #[test]
    fn lua_deny_trust_types_numeric_option_two() {
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(&extension, "deny_trust", serde_json::json!({}));

        assert_eq!(
            plan.actions,
            vec![
                Action::Text("2".to_string()),
                Action::Key(crate::action::Key::Enter),
            ],
        );
    }

    #[test]
    fn lua_dismiss_welcome_sends_single_enter() {
        // The first-launch welcome panel traps Enter; the plugin exposes
        // `dismiss_welcome` as a single-Enter action so callers don't have
        // to drop down to session.input to clear it.
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(&extension, "dismiss_welcome", serde_json::json!({}));
        assert_eq!(plan.actions, vec![Action::Key(crate::action::Key::Enter)]);
        assert!(
            plan.last_intent.is_none(),
            "dismiss_welcome is non-mutating; last_intent must stay as-is"
        );
    }

    /// Auto-enrolling classifier regression test.
    ///
    /// For every `*.txt` fixture under `tests/fixtures/claude_code/`, this
    /// test loads a sibling `<name>.expected.json` describing the expected
    /// state, evidence, optional `last_intent`, and a confidence floor, and
    /// asserts the Lua-backed classifier still matches.
    ///
    /// Adding a new fixture is now a documentation-only change: drop the
    /// two files (text + JSON) into the fixtures directory and this test
    /// picks them up automatically. Fixtures without a matching
    /// `.expected.json` are skipped with a warning to leave room for
    /// exploratory captures that have not been classified yet.
    #[test]
    fn classifier_matches_sanitized_claude_code_fixtures() {
        #[derive(serde::Deserialize)]
        struct Expectation {
            state: String,
            evidence: String,
            #[serde(default)]
            last_intent: Option<String>,
            #[serde(default = "default_min_confidence")]
            min_confidence: f32,
        }

        fn default_min_confidence() -> f32 {
            0.6
        }

        let fixtures_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("claude_code");

        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&fixtures_dir)
            .unwrap_or_else(|err| panic!("read fixtures dir {}: {err}", fixtures_dir.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("txt"))
            .collect();
        entries.sort();

        assert!(
            !entries.is_empty(),
            "no .txt fixtures found in {}",
            fixtures_dir.display()
        );

        let extension = claude_plugin().expect("load plugin");
        let mut asserted = 0usize;

        for (index, txt_path) in entries.into_iter().enumerate() {
            let fixture_name = txt_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| txt_path.display().to_string());

            let expected_path = txt_path.with_extension("expected.json");
            if !expected_path.exists() {
                println!(
                    "skipping fixture {fixture_name}: missing sibling {}",
                    expected_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("<expected>.json")
                );
                continue;
            }

            let fixture_body = std::fs::read_to_string(&txt_path)
                .unwrap_or_else(|err| panic!("read fixture {fixture_name}: {err}"));
            let expected_raw = std::fs::read_to_string(&expected_path)
                .unwrap_or_else(|err| panic!("read expectations for {fixture_name}: {err}"));
            let expectation: Expectation = serde_json::from_str(&expected_raw)
                .unwrap_or_else(|err| panic!("parse expectations for {fixture_name}: {err}"));

            let expected_state = state_from_name(&expectation.state).unwrap_or_else(|| {
                panic!(
                    "unknown expected state {:?} in expectations for {fixture_name}",
                    expectation.state
                )
            });
            let last_intent = expectation.last_intent.as_deref().map(|name| {
                state_from_name(name).unwrap_or_else(|| {
                    panic!("unknown last_intent {name:?} in expectations for {fixture_name}")
                })
            });

            let state = classify_state(
                &extension,
                &fixture_body,
                "",
                index as u64,
                last_intent,
                Some(COMPLETED_TURN_STABLE_MS),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "classify fixture {fixture_name} via Lua plugin: {err}\n--- fixture body ---\n{fixture_body}"
                )
            });

            assert_eq!(
                state.state, expected_state,
                "fixture {fixture_name} classified as {:?} with evidence: {}",
                state.state, state.evidence
            );
            assert_eq!(
                state.evidence, expectation.evidence,
                "fixture {fixture_name} evidence mismatch"
            );
            assert!(
                state.confidence >= expectation.min_confidence,
                "fixture {fixture_name} confidence {} below floor {}",
                state.confidence,
                expectation.min_confidence
            );
            asserted += 1;
        }

        assert!(
            asserted > 0,
            "no fixtures had sibling .expected.json files under {}",
            fixtures_dir.display()
        );
    }

    #[test]
    fn unknown_extension_state_translates_to_error_with_state_in_evidence() {
        let snapshot = ExtensionStateSnapshot {
            state: "no-such-state".to_string(),
            confidence: 0.3,
            evidence: "from plugin".to_string(),
            sequence: 42,
            candidates: vec![StateCandidate {
                state: "ready".to_string(),
                confidence: 0.1,
            }],
        };
        let claude: ClaudeCodeStateSnapshot = snapshot.into();
        assert_eq!(claude.state, ClaudeCodeState::Error);
        assert!(claude.evidence.contains("no-such-state"));
        assert!(claude.evidence.contains("from plugin"));
        assert_eq!(claude.sequence, 42);
    }

    #[test]
    fn unknown_extension_state_with_empty_evidence_still_names_the_unknown_state() {
        // The `if evidence.is_empty()` branch in From<ExtensionStateSnapshot>
        // for ClaudeCodeStateSnapshot is reachable when a plugin returns an
        // unfamiliar state name without an evidence message. Lock the
        // "no trailing colon" formatting so a future refactor doesn't
        // accidentally produce `unknown extension state `x`: ` with a
        // dangling separator.
        let snapshot = ExtensionStateSnapshot {
            state: "wat".to_string(),
            confidence: 0.0,
            evidence: String::new(),
            sequence: 1,
            candidates: Vec::new(),
        };
        let claude: ClaudeCodeStateSnapshot = snapshot.into();
        assert_eq!(claude.state, ClaudeCodeState::Error);
        assert_eq!(claude.evidence, "unknown extension state `wat`");
    }
}
