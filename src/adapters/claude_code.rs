use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

const COMPLETED_TURN_STABLE_MS: u64 = 300;

use serde::{Deserialize, Serialize};

use crate::action::Action;
use crate::error::Result;
use crate::lua_plugin::LuaPlugin;
use crate::matcher::Matcher;
use crate::plugin::claude_code_manifest;
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

/// Interactive Claude Code adapter backed by a generic ptywright session.
pub struct ClaudeCodeAdapter {
    session: Session,
    plugin: LuaPlugin,
    last_intent: Option<ClaudeCodeState>,
}

impl ClaudeCodeAdapter {
    /// Spawn interactive Claude Code in a PTY.
    pub fn start(config: ClaudeCodeConfig) -> Result<Self> {
        let session = Session::spawn(SessionConfig::new(config.target()))?;
        Ok(Self {
            session,
            plugin: claude_plugin()?,
            last_intent: Some(ClaudeCodeState::Starting),
        })
    }

    /// Wrap an existing session. Useful for tests or externally managed sessions.
    pub fn from_session(session: Session) -> Result<Self> {
        Ok(Self {
            session,
            plugin: claude_plugin()?,
            last_intent: None,
        })
    }

    /// Access the underlying generic session.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Classify current Claude Code state from visible screen and transcript evidence.
    #[must_use]
    pub fn state(&self) -> ClaudeCodeStateSnapshot {
        self.try_state().unwrap_or_else(|error| {
            state_snapshot(
                ClaudeCodeState::PluginError,
                0.0,
                format!("Claude Code Lua plugin failed: {error}"),
                self.session.sequence(),
            )
        })
    }

    /// Classify current Claude Code state and surface Lua plugin failures.
    pub fn try_state(&self) -> Result<ClaudeCodeStateSnapshot> {
        let snapshot = self.session.snapshot();
        let transcript = self.session.transcript();
        classify_state(
            &self.plugin,
            &snapshot.plain_text,
            &transcript,
            snapshot.sequence,
            self.last_intent,
            None,
        )
    }

    /// Send a prompt to the interactive Claude Code TUI.
    pub fn send_prompt(&mut self, prompt: impl AsRef<str>) -> Result<ClaudeCodeStateSnapshot> {
        let plan: ActionPlan = self.plugin.call(
            "send_prompt",
            &serde_json::json!({ "prompt": prompt.as_ref() }),
        )?;
        self.apply_plan_with_required_intent(&plan, "send_prompt")?;
        self.try_state()
    }

    /// Wait until Claude appears to need user input, approval, or has completed a turn.
    pub fn wait_turn(&self, timeout: Duration) -> Result<ClaudeCodeStateSnapshot> {
        let matcher: Matcher = self.plugin.call(
            "wait_turn_matcher",
            &serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
        )?;
        let result = self.session.wait_for(&matcher, timeout)?;
        classify_state(
            &self.plugin,
            &result.snapshot.plain_text,
            &result.transcript_tail,
            result.sequence,
            self.last_intent,
            Some(COMPLETED_TURN_STABLE_MS),
        )
    }

    /// Approve the current Claude Code prompt using the Lua adapter's action plan.
    pub fn approve(&self) -> Result<()> {
        let plan: ActionPlan = self.plugin.call("approve", &serde_json::json!({}))?;
        self.apply_actions(&plan.actions)
    }

    /// Deny the current Claude Code prompt using the Lua adapter's action plan.
    pub fn deny(&self) -> Result<()> {
        let plan: ActionPlan = self.plugin.call("deny", &serde_json::json!({}))?;
        self.apply_actions(&plan.actions)
    }

    /// Cancel the current turn with the Lua adapter's action plan.
    pub fn cancel(&mut self) -> Result<ClaudeCodeStateSnapshot> {
        let plan: ActionPlan = self.plugin.call("cancel", &serde_json::json!({}))?;
        self.apply_plan_with_required_intent(&plan, "cancel")?;
        self.try_state()
    }

    fn apply_plan_with_required_intent(&mut self, plan: &ActionPlan, method: &str) -> Result<()> {
        self.apply_actions(&plan.actions)?;
        self.last_intent = Some(plan.last_intent.ok_or_else(|| {
            crate::Error::Lua(format!(
                "Claude Code Lua method `{method}` did not return last_intent"
            ))
        })?);
        Ok(())
    }

    fn apply_actions(&self, actions: &[Action]) -> Result<()> {
        for action in actions {
            self.session.send(action.clone())?;
        }
        Ok(())
    }
}

/// Bottom rows of the rendered screen treated as the Claude Code status bar.
///
/// Used to split screen text into `body_text` (content area) and `status_text`
/// (status bar) before handing it to the Lua classifier so that benign status
/// strings like `⏵⏵ bypass permissions on (shift+tab to cycle)` cannot
/// false-positive on the `permissions` substring match in
/// `plugins/claude-code/main.lua`.
const STATUS_BAR_ROWS: usize = 3;

fn split_status_bar(screen: &str, status_rows: usize) -> (String, String) {
    let lines: Vec<&str> = screen.split('\n').collect();
    if lines.is_empty() {
        return (String::new(), String::new());
    }
    // Only split a screen that's tall enough to actually have a body + status
    // bar. The Claude Code status bar pattern (separator + status rows at the
    // bottom) only manifests on full-height TUI screens. Short fixtures and
    // small windows are entirely body — splitting them would shove the only
    // content into status_text and break classification.
    if lines.len() <= status_rows * 2 {
        return (screen.to_string(), String::new());
    }
    let cutoff = lines.len() - status_rows;
    let body = lines[..cutoff].join("\n");
    let status = lines[cutoff..].join("\n");
    (body, status)
}

fn classify_state(
    plugin: &LuaPlugin,
    screen: &str,
    transcript: &str,
    sequence: u64,
    last_intent: Option<ClaudeCodeState>,
    stable_ms: Option<u64>,
) -> Result<ClaudeCodeStateSnapshot> {
    let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
    plugin.call(
        "classify",
        &ClassifyInput {
            screen,
            body_text: &body_text,
            status_text: &status_text,
            transcript,
            sequence,
            last_intent: last_intent.map(state_name).transpose()?,
            stable_ms,
            completed_turn_stable_ms: COMPLETED_TURN_STABLE_MS,
        },
    )
}

fn state_snapshot(
    state: ClaudeCodeState,
    confidence: f32,
    evidence: impl Into<String>,
    sequence: u64,
) -> ClaudeCodeStateSnapshot {
    ClaudeCodeStateSnapshot {
        state,
        confidence,
        evidence: evidence.into(),
        sequence,
    }
}

#[derive(Debug, Serialize)]
struct ClassifyInput<'a> {
    /// Full visible screen text. Kept for backward compatibility with any Lua
    /// classifier path that wants the unsegmented view.
    screen: &'a str,
    /// Screen text with the bottom status-bar rows removed. Permission/plan
    /// classification should match on `body_text` to avoid false-positives
    /// from status strings like "bypass permissions on".
    body_text: &'a str,
    /// Only the bottom status-bar rows. Available for plugins that want to
    /// inspect the status bar explicitly (e.g. detect "[ctx: 26% used]").
    status_text: &'a str,
    transcript: &'a str,
    sequence: u64,
    last_intent: Option<String>,
    stable_ms: Option<u64>,
    completed_turn_stable_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ActionPlan {
    actions: Vec<Action>,
    #[serde(default)]
    last_intent: Option<ClaudeCodeState>,
}

fn claude_plugin() -> Result<LuaPlugin> {
    LuaPlugin::trusted(
        &claude_code_manifest(),
        include_str!("../../plugins/claude-code/main.lua"),
    )
}

fn state_name(state: ClaudeCodeState) -> Result<String> {
    serde_json::to_value(state)?
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| crate::Error::Lua("Claude Code state did not serialize to a string".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let plan: ActionPlan = claude_plugin()
            .expect("load built-in Claude Code Lua plugin")
            .call(
                "send_prompt",
                &serde_json::json!({ "prompt": "hello Claude" }),
            )
            .expect("load prompt action plan from Lua");

        assert_eq!(
            plan.actions,
            vec![
                Action::Paste("hello Claude".to_string()),
                Action::Key(crate::Key::Enter),
            ]
        );
        assert_eq!(plan.last_intent, Some(ClaudeCodeState::PromptSubmitted));
    }

    #[test]
    fn lua_plugin_supplies_turn_wait_matcher() {
        let matcher: Matcher = claude_plugin()
            .expect("load built-in Claude Code Lua plugin")
            .call(
                "wait_turn_matcher",
                &serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
            )
            .expect("load wait matcher from Lua");

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
        let matcher: Matcher = claude_plugin()
            .expect("load built-in Claude Code Lua plugin")
            .call(
                "wait_turn_matcher",
                &serde_json::json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
            )
            .expect("load wait matcher from Lua");
        let snapshot = crate::ScreenSnapshot {
            size: TerminalSize::new(3, 20),
            cursor: crate::CursorState {
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
    fn split_status_bar_handles_short_screens_without_panic() {
        // Empty input — both halves empty.
        assert_eq!(split_status_bar("", 3), (String::new(), String::new()));
        // Short screens (<= status_rows * 2 lines) stay fully as body; we do
        // not strip content from small fixtures that don't actually have a
        // bottom status bar.
        assert_eq!(
            split_status_bar("only line", 3),
            ("only line".to_string(), String::new()),
        );
        let (body, status) = split_status_bar("a\nb", 3);
        assert_eq!(body, "a\nb");
        assert_eq!(status, "");
    }

    #[test]
    fn lua_cancel_sets_cancelling_intent() {
        let plan: ActionPlan = claude_plugin()
            .expect("load built-in Claude Code Lua plugin")
            .call("cancel", &serde_json::json!({}))
            .expect("load cancel action plan from Lua");

        assert_eq!(plan.actions, vec![Action::Interrupt]);
        assert_eq!(plan.last_intent, Some(ClaudeCodeState::Cancelling));
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
}
