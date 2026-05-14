use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::action::{Action, Key};
use crate::error::Result;
use crate::matcher::Matcher;
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
    last_intent: Option<ClaudeCodeState>,
}

impl ClaudeCodeAdapter {
    /// Spawn interactive Claude Code in a PTY.
    pub fn start(config: ClaudeCodeConfig) -> Result<Self> {
        let session = Session::spawn(SessionConfig::new(config.target()))?;
        Ok(Self {
            session,
            last_intent: Some(ClaudeCodeState::Starting),
        })
    }

    /// Wrap an existing session. Useful for tests or externally managed sessions.
    #[must_use]
    pub fn from_session(session: Session) -> Self {
        Self {
            session,
            last_intent: None,
        }
    }

    /// Access the underlying generic session.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Classify current Claude Code state from visible screen and transcript evidence.
    #[must_use]
    pub fn state(&self) -> ClaudeCodeStateSnapshot {
        let snapshot = self.session.snapshot();
        let transcript = self.session.transcript();
        classify_state(
            &snapshot.plain_text,
            &transcript,
            snapshot.sequence,
            self.last_intent,
        )
    }

    /// Send a prompt to the interactive Claude Code TUI.
    pub fn send_prompt(&mut self, prompt: impl AsRef<str>) -> Result<ClaudeCodeStateSnapshot> {
        self.session
            .send(Action::Paste(prompt.as_ref().to_string()))?;
        self.session.send(Action::Key(Key::Enter))?;
        self.last_intent = Some(ClaudeCodeState::PromptSubmitted);
        Ok(self.state())
    }

    /// Wait until Claude appears to need user input, approval, or has completed a turn.
    pub fn wait_turn(&self, timeout: Duration) -> Result<ClaudeCodeStateSnapshot> {
        let matcher = Matcher::Any(vec![
            Matcher::ContainsText("Do you want to proceed".to_string()),
            Matcher::ContainsText("Approve".to_string()),
            Matcher::ContainsText("Allow".to_string()),
            Matcher::ContainsText("❯".to_string()),
            Matcher::ContainsText(">".to_string()),
        ]);
        let result = self.session.wait_for(&matcher, timeout)?;
        Ok(classify_state(
            &result.snapshot.plain_text,
            &result.transcript_tail,
            result.sequence,
            self.last_intent,
        ))
    }

    /// Approve the current Claude Code prompt using Enter.
    pub fn approve(&self) -> Result<()> {
        self.session.send(Action::Key(Key::Enter))
    }

    /// Deny the current Claude Code prompt using Escape.
    pub fn deny(&self) -> Result<()> {
        self.session.send(Action::Key(Key::Escape))
    }

    /// Cancel the current turn with Ctrl-C.
    pub fn cancel(&mut self) -> Result<ClaudeCodeStateSnapshot> {
        self.session.send(Action::Interrupt)?;
        self.last_intent = Some(ClaudeCodeState::Cancelling);
        Ok(self.state())
    }
}

fn classify_state(
    screen: &str,
    transcript: &str,
    sequence: u64,
    last_intent: Option<ClaudeCodeState>,
) -> ClaudeCodeStateSnapshot {
    let combined = format!("{screen}\n{transcript}");
    let lower = combined.to_lowercase();

    if lower.trim().is_empty() {
        return state_snapshot(
            last_intent.unwrap_or(ClaudeCodeState::Starting),
            0.35,
            "no screen evidence yet",
            sequence,
        );
    }

    if lower.contains("plan") && contains_any(&lower, &["approve", "accept", "proceed"]) {
        return state_snapshot(
            ClaudeCodeState::WaitingForPlanApproval,
            0.78,
            "plan approval text detected",
            sequence,
        );
    }

    if contains_any(
        &lower,
        &["do you want to proceed", "permission", "allow", "approve"],
    ) {
        return state_snapshot(
            ClaudeCodeState::WaitingForPermission,
            0.82,
            "permission or approval prompt text detected",
            sequence,
        );
    }

    if contains_any(
        &lower,
        &["esc to interrupt", "thinking", "thinking…", "thinking..."],
    ) {
        return state_snapshot(
            ClaudeCodeState::Thinking,
            0.72,
            "thinking indicator detected",
            sequence,
        );
    }

    if contains_any(
        &lower,
        &[
            "error:",
            "request failed",
            "failed to",
            "try again",
            "retry",
        ],
    ) {
        return state_snapshot(
            ClaudeCodeState::Error,
            0.7,
            "error or retry text detected",
            sequence,
        );
    }

    if contains_any(
        &lower,
        &["what would you like", "how can i help", "type a message"],
    ) {
        return state_snapshot(
            ClaudeCodeState::Ready,
            0.74,
            "ready prompt text detected",
            sequence,
        );
    }

    if screen.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == ">" || trimmed.ends_with(" >") || trimmed.starts_with('❯')
    }) {
        let state = if last_intent == Some(ClaudeCodeState::PromptSubmitted) {
            ClaudeCodeState::CompletedTurn
        } else {
            ClaudeCodeState::WaitingForUserInput
        };
        return state_snapshot(state, 0.62, "input prompt glyph detected", sequence);
    }

    state_snapshot(
        last_intent.unwrap_or(ClaudeCodeState::Ready),
        0.2,
        "no Claude Code-specific evidence detected",
        sequence,
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

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
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
    fn classifier_detects_permission_prompt() {
        let state = classify_state("Do you want to proceed? Allow tool use", "", 3, None);

        assert_eq!(state.state, ClaudeCodeState::WaitingForPermission);
        assert!(state.confidence > 0.8);
    }

    #[test]
    fn classifier_detects_plan_approval() {
        let state = classify_state("Plan ready. Approve plan to proceed", "", 4, None);

        assert_eq!(state.state, ClaudeCodeState::WaitingForPlanApproval);
    }

    #[test]
    fn classifier_detects_thinking() {
        let state = classify_state("Thinking... Esc to interrupt", "", 5, None);

        assert_eq!(state.state, ClaudeCodeState::Thinking);
    }

    #[test]
    fn classifier_detects_completed_turn_after_prompt_submission() {
        let state = classify_state(
            "work completed\n>",
            "",
            6,
            Some(ClaudeCodeState::PromptSubmitted),
        );

        assert_eq!(state.state, ClaudeCodeState::CompletedTurn);
    }

    #[test]
    fn classifier_matches_sanitized_claude_code_fixtures() {
        let fixtures = [
            (
                include_str!("../../tests/fixtures/claude_code/ready.txt"),
                None,
                ClaudeCodeState::Ready,
            ),
            (
                include_str!("../../tests/fixtures/claude_code/thinking.txt"),
                None,
                ClaudeCodeState::Thinking,
            ),
            (
                include_str!("../../tests/fixtures/claude_code/permission.txt"),
                None,
                ClaudeCodeState::WaitingForPermission,
            ),
            (
                include_str!("../../tests/fixtures/claude_code/plan_approval.txt"),
                None,
                ClaudeCodeState::WaitingForPlanApproval,
            ),
            (
                include_str!("../../tests/fixtures/claude_code/completed.txt"),
                Some(ClaudeCodeState::PromptSubmitted),
                ClaudeCodeState::CompletedTurn,
            ),
            (
                include_str!("../../tests/fixtures/claude_code/error.txt"),
                None,
                ClaudeCodeState::Error,
            ),
        ];

        for (index, (fixture, last_intent, expected)) in fixtures.into_iter().enumerate() {
            let state = classify_state(fixture, "", index as u64, last_intent);
            assert_eq!(
                state.state, expected,
                "fixture {index} classified with evidence: {}",
                state.evidence
            );
            assert!(state.confidence >= 0.6);
        }
    }
}
