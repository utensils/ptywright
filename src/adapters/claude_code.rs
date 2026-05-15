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
        let extension = claude_plugin().expect("load built-in Claude Code Lua plugin");
        let plan = lua_call_plan(
            &extension,
            "send_prompt",
            serde_json::json!({ "prompt": "hello Claude" }),
        );

        assert_eq!(
            plan.actions,
            vec![
                Action::Paste("hello Claude".to_string()),
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
    fn classifier_matches_sanitized_claude_code_fixtures() {
        let fixtures = [
            (
                include_str!("../../tests/fixtures/claude_code/ready.txt"),
                None,
                ClaudeCodeState::Ready,
                "ready prompt text detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/thinking.txt"),
                None,
                ClaudeCodeState::Thinking,
                "active work indicator detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/permission.txt"),
                None,
                ClaudeCodeState::WaitingForPermission,
                "permission or approval prompt text detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/plan_approval.txt"),
                None,
                ClaudeCodeState::WaitingForPlanApproval,
                "plan approval text detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/completed.txt"),
                Some(ClaudeCodeState::PromptSubmitted),
                ClaudeCodeState::CompletedTurn,
                "stable input prompt after prompt submission",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/tool_use.txt"),
                None,
                ClaudeCodeState::Thinking,
                "active work indicator detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/streaming_response.txt"),
                None,
                ClaudeCodeState::Thinking,
                "active work indicator detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/interrupted.txt"),
                Some(ClaudeCodeState::Cancelling),
                ClaudeCodeState::WaitingForUserInput,
                "input prompt glyph detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/permission_bash.txt"),
                None,
                ClaudeCodeState::WaitingForPermission,
                "permission or approval prompt text detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/plan_variant.txt"),
                None,
                ClaudeCodeState::WaitingForPlanApproval,
                "plan approval text detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/usage.txt"),
                Some(ClaudeCodeState::PromptSubmitted),
                ClaudeCodeState::CompletedTurn,
                "stable usage screen detected",
            ),
            (
                include_str!("../../tests/fixtures/claude_code/error.txt"),
                None,
                ClaudeCodeState::Error,
                "visible error banner detected",
            ),
            // Regression: Claude Code 2.1.142 puts `⏵⏵ bypass permissions on
            // (shift+tab to cycle)` in the bottom status bar on an idle input
            // screen. Before status-bar splitting the classifier matched the
            // `permission` substring and returned `WaitingForPermission` at
            // 0.84 confidence on an idle screen. The fix in
            // `split_status_bar` + body/status plumbing must keep this
            // classified as `WaitingForUserInput` (idle prompt glyph) rather
            // than a permission dialog.
            (
                include_str!("../../tests/fixtures/claude_code/idle_bypass_permissions.txt"),
                None,
                ClaudeCodeState::WaitingForUserInput,
                "input prompt glyph detected",
            ),
        ];

        for (index, (fixture, last_intent, expected, evidence)) in fixtures.into_iter().enumerate()
        {
            let state = classify_state(
                &claude_plugin().expect("load plugin"),
                fixture,
                "",
                index as u64,
                last_intent,
                Some(COMPLETED_TURN_STABLE_MS),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "classify fixture {index} via Lua plugin: {err}\n--- fixture body ---\n{fixture}"
                )
            });
            assert_eq!(
                state.state, expected,
                "fixture {index} classified with evidence: {}",
                state.evidence
            );
            assert_eq!(state.evidence, evidence);
            assert!(state.confidence >= 0.6);
        }
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
}
