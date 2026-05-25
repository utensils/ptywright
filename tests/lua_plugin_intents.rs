//! Per-intent regression tests for the built-in claude-code Lua plugin.
//!
//! Drives the plugin directly through the generic [`Extension`] /
//! [`ExtensionHandle`] surface — no application-specific Rust wrapper sits
//! between the test and the Lua code. The host-side patterns these tests
//! pin down (action plans for `send_prompt`/`approve`/`deny`/`cancel`/
//! `approve_trust`/`deny_trust`/`dismiss_welcome`, the `wait_turn_matcher`
//! boundary anchors, the `cancelling` hold-state) are the contract any new
//! TUI plugin would follow, so the tests double as a reference for plugin
//! authors.

use std::time::Duration;

use serde_json::json;

use ptywright::action::Key;
use ptywright::extension::{
    ActionPlan, ClassifyContext, Extension, ExtensionHandle, ExtensionStateSnapshot, LuaExtension,
    STATUS_BAR_ROWS, split_status_bar,
};
use ptywright::matcher::Matcher;
use ptywright::screen::{CursorState, ScreenSnapshot};
use ptywright::target::TerminalSize;
use ptywright::{Action, MatcherContext, StreamText};

const COMPLETED_TURN_STABLE_MS: u64 = 300;

fn claude_plugin() -> LuaExtension {
    LuaExtension::built_in("claude-code").expect("load built-in claude-code Lua plugin")
}

fn classify_state(
    extension: &LuaExtension,
    screen: &str,
    sequence: u64,
    last_intent: Option<&str>,
    stable_ms: Option<u64>,
) -> ExtensionStateSnapshot {
    classify_with_markers(
        extension,
        screen,
        sequence,
        last_intent,
        stable_ms,
        &std::collections::BTreeMap::new(),
        0,
    )
}

fn classify_with_markers(
    extension: &LuaExtension,
    screen: &str,
    sequence: u64,
    last_intent: Option<&str>,
    stable_ms: Option<u64>,
    markers: &std::collections::BTreeMap<String, u64>,
    cursor: u64,
) -> ExtensionStateSnapshot {
    let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
    let ctx = ClassifyContext {
        screen,
        body_text: &body_text,
        status_text: &status_text,
        transcript: "",
        sequence,
        last_intent,
        stable_ms,
        completed_turn_stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        markers,
        cursor,
    };
    extension.classify(&ctx).expect("classify via Lua plugin")
}

fn classify_with_transcript(
    extension: &LuaExtension,
    screen: &str,
    transcript: &str,
    sequence: u64,
    last_intent: Option<&str>,
    stable_ms: Option<u64>,
) -> ExtensionStateSnapshot {
    let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
    let markers = std::collections::BTreeMap::new();
    let ctx = ClassifyContext {
        screen,
        body_text: &body_text,
        status_text: &status_text,
        transcript,
        sequence,
        last_intent,
        stable_ms,
        completed_turn_stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        markers: &markers,
        cursor: 0,
    };
    extension.classify(&ctx).expect("classify via Lua plugin")
}

fn plan(extension: &LuaExtension, intent: &str, params: serde_json::Value) -> ActionPlan {
    extension.plan(intent, &params).expect("plan from Lua")
}

fn wait_matcher(extension: &LuaExtension, intent: &str, params: serde_json::Value) -> Matcher {
    extension
        .wait_matcher(intent, &params)
        .expect("matcher from Lua")
}

#[test]
fn describe_exposes_claude_code_catalog() {
    let extension = claude_plugin();
    let catalog = extension
        .plugin()
        .call_value("describe", &json!({}))
        .expect("describe catalog");

    let intents = catalog["intents"].as_array().expect("intents array");
    let intent_names: Vec<&str> = intents.iter().filter_map(|i| i["name"].as_str()).collect();
    for required in &[
        "send_prompt",
        "steer",
        "attach_file",
        "approve",
        "deny",
        "choose_option",
        "approve_trust",
        "deny_trust",
        "dismiss_welcome",
        "expand",
        "model_effort_left",
        "model_effort_right",
        "slash_command",
        "cancel",
        "force_cancel",
        "key",
    ] {
        assert!(
            intent_names.contains(required),
            "intent `{required}` missing from describe(): {intent_names:?}"
        );
    }

    let wait_matchers = catalog["wait_matchers"]
        .as_array()
        .expect("wait_matchers array");
    let wait_matcher_names: Vec<&str> = wait_matchers
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    for required in &["wait_turn_matcher", "wait_cancel_settled_matcher"] {
        assert!(
            wait_matcher_names.contains(required),
            "wait matcher `{required}` missing from describe(): {wait_matcher_names:?}"
        );
    }

    let states = catalog["states"].as_array().expect("states array");
    let state_names: Vec<&str> = states.iter().filter_map(|i| i["name"].as_str()).collect();
    for required in &[
        "starting",
        "ready",
        "waiting_for_login",
        "waiting_for_trust",
        "waiting_for_model_select",
        "waiting_for_enter_plan_mode",
        "waiting_for_plan_approval",
        "waiting_for_permission",
        "waiting_for_external_editor",
        "usage_screen",
        "local_ui_screen",
        "waiting_for_user_input",
        "thinking",
        "cancelling",
        "completed_turn",
        "error",
    ] {
        assert!(
            state_names.contains(required),
            "state `{required}` missing from describe(): {state_names:?}"
        );
    }
}

#[test]
fn welcome_panel_does_not_downgrade_completed_turn_when_prompt_submitted() {
    // Reproduces the Claude Code 2.1.143 behaviour where the post-trust
    // welcome panel stays rendered as visual residue even after the user
    // has submitted a prompt. Without the `last_intent == prompt_submitted`
    // gate in the welcome branch, this screen oscillates between `thinking`
    // (on ticks where the spinner glyph is captured) and `starting` (on
    // ticks between spinner frames) — making turn-boundary polling
    // unreliable. The fix: once a prompt has been submitted, the welcome
    // chrome is stale and the classifier falls through to the regular
    // input-prompt / completed-turn branches.
    let extension = claude_plugin();
    let screen = "\
╭─── Claude Code v2.1.143 ────────────────────────────────────────────╮
│ Welcome back James!     │ Tips for getting started                  │
│  ▐▛███▜▌                │ Ask Claude to create a new app            │
│  ▝▜█████▛▘              │ What's new                                │
│  ▘▘ ▝▝                  │ /release-notes for more                   │
╰─────────────────────────────────────────────────────────────────────╯

❯ What is 2+2? Reply with just the digit.

⏺ 4

✻ Brewed for 0.4s

❯
────────────────────────────────────────────────────────────────────────
  user @ host /workspace                                  [Haiku 4.5]
  ⏵⏵ auto mode on (shift+tab to cycle)
";

    // With stable_ms >= the stability window — adapter.wait path.
    let st = classify_state(
        &extension,
        screen,
        7,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );
    assert_eq!(
        st.state, "completed_turn",
        "welcome chrome must not downgrade to `starting` post-submit (stable path); got state={} evidence={}",
        st.state, st.evidence
    );

    // Without stable_ms — adapter.state poll path. The screen has the
    // answer bullet (`⏺ 4`), the tea-verb completion marker
    // (`✻ Brewed for 0.4s`), and the empty input prompt (`❯` alone on
    // its row), so the poll-path `completed_turn` branch fires with
    // confidence 0.7. The important property the welcome-residue test
    // locks here is that the welcome chrome does NOT downgrade this to
    // `starting`; with `last_intent == "prompt_submitted"` the welcome
    // detection is suppressed.
    let st_poll = classify_state(&extension, screen, 7, Some("prompt_submitted"), None);
    assert_eq!(
        st_poll.state, "completed_turn",
        "welcome chrome must not downgrade post-submit on state-poll path; got state={} evidence={}",
        st_poll.state, st_poll.evidence
    );
    assert_eq!(
        st_poll.evidence, "completion marker plus input prompt visible without active work",
        "poll-path completed_turn must own this screen"
    );

    // Sanity: when last_intent is empty (still in the welcome-dismissal
    // window) the welcome detection still wins.
    let st_pre = classify_state(&extension, screen, 7, None, Some(COMPLETED_TURN_STABLE_MS));
    assert_eq!(
        st_pre.state, "starting",
        "before prompt submission, welcome panel still classifies as `starting`; got state={}",
        st_pre.state
    );
}

#[test]
fn steer_plan_uses_bracketed_paste_without_setting_prompt_submitted_intent() {
    // Mid-turn steering injects a follow-up prompt while Claude is still
    // thinking. The action shape is identical to `send_prompt` (bracketed
    // paste + Enter) but `last_intent` MUST NOT flip to `prompt_submitted`
    // — that's the marker the classifier uses to decide a `completed_turn`
    // can fire when the screen settles. Treating steering as the first
    // half of a fresh turn would cause `adapter.wait` to mistake a
    // stable-thinking screen for "turn complete" the moment Claude is
    // just slow on the original turn.
    let extension = claude_plugin();
    let plan = plan(
        &extension,
        "steer",
        json!({ "prompt": "actually use /tmp" }),
    );

    assert_eq!(
        plan.actions,
        vec![
            Action::BracketedPaste("actually use /tmp".to_string()),
            Action::Key(Key::Enter),
        ]
    );
    assert!(
        plan.last_intent.is_none(),
        "steer is mid-turn — last_intent must not flip to prompt_submitted"
    );
}

#[test]
fn send_prompt_plan_dismisses_then_pastes_then_submits() {
    // The plan emits FIVE actions in this exact order:
    //   1. Enter  — dismiss any first-keypress interceptor (welcome
    //      panel, compact-launch view). On a clean empty input box
    //      Claude treats this as a no-op submit.
    //   2. MarkTranscript("turn_start") — stamp a turn-boundary marker
    //      so callers can later slice the per-turn output via
    //      `Session::transcript_slice`. The matching `turn_end` mark
    //      fires from the classifier's `completed_turn` branch.
    //   3. StreamText(prompt) — Claude Code v2.1.150 can swallow bracketed
    //      paste in Chrome-enabled interactive mode, while huge raw writes
    //      can render as collapsed paste placeholders.
    //   4. Enter — submit the now-populated input box.
    //   5. Enter — recovery submit for Claude Code builds that accept
    //      the streamed text but leave it editable after the first trailing
    //      Enter.
    //
    // Without action #1, the text can land in Claude's first-keypress
    // interceptor on a fresh launch instead of the input box. Locking
    // the five-action sequence here so a
    // future plugin edit can't silently regress.
    let extension = claude_plugin();
    let plan = plan(
        &extension,
        "send_prompt",
        json!({ "prompt": "hello Claude" }),
    );

    assert_eq!(
        plan.actions,
        vec![
            Action::Key(Key::Enter),
            Action::MarkTranscript {
                label: "turn_start".to_string()
            },
            Action::StreamText(StreamText {
                text: "hello Claude".to_string(),
                chunk_chars: None,
                delay_ms: None,
            }),
            Action::Key(Key::Enter),
            Action::Key(Key::Enter),
        ]
    );
    assert_eq!(plan.last_intent.as_deref(), Some("prompt_submitted"));
}

/// `expand` is the named intent for Claude Code's Ctrl+O binding —
/// toggles expansion of the collapsible tool-progress row under the
/// cursor. Generic `key("ctrl_o")` works too; this is the
/// discoverable named-intent affordance.
#[test]
fn expand_plan_sends_ctrl_o_with_no_intent() {
    let extension = claude_plugin();
    let plan = plan(&extension, "expand", json!({}));

    assert_eq!(plan.actions, vec![Action::Key(Key::CtrlO)]);
    assert!(
        plan.last_intent.is_none(),
        "expand toggles a UI affordance, not a turn — last_intent must stay unset"
    );
}

/// Slash commands open UI panels (modal / inline) — they're not
/// conversation turns. The plugin pastes `/<name>` and presses Enter
/// without flipping `last_intent`, so the classifier reads whatever
/// the slash command rendered (the `/model` picker, `/usage` screen,
/// `/help` panel, plain back-at-idle for `/btw` / `/clear`, etc.)
/// instead of being lied to about a turn in flight.
#[test]
fn slash_command_streams_token_and_presses_enter_without_intent() {
    let extension = claude_plugin();

    // Bare name — plugin adds the leading slash.
    let bare = plan(&extension, "slash_command", json!({ "command": "btw" }));
    assert_eq!(
        bare.actions,
        vec![
            Action::StreamText(StreamText {
                text: "/btw".to_string(),
                chunk_chars: None,
                delay_ms: None,
            }),
            Action::Key(Key::Enter),
        ]
    );
    assert!(
        bare.last_intent.is_none(),
        "slash command is not a turn — must not flip last_intent"
    );

    // Already-prefixed name — plugin passes through.
    let prefixed = plan(&extension, "slash_command", json!({ "command": "/btw" }));
    assert_eq!(
        prefixed.actions,
        vec![
            Action::StreamText(StreamText {
                text: "/btw".to_string(),
                chunk_chars: None,
                delay_ms: None,
            }),
            Action::Key(Key::Enter),
        ]
    );

    // `name` is accepted as an alias for `command` so callers can use
    // either field idiomatically.
    let via_name = plan(&extension, "slash_command", json!({ "name": "usage" }));
    assert_eq!(
        via_name.actions,
        vec![
            Action::StreamText(StreamText {
                text: "/usage".to_string(),
                chunk_chars: None,
                delay_ms: None,
            }),
            Action::Key(Key::Enter),
        ]
    );

    // Startup probes can opt into a leading Enter to clear Claude Code's
    // first-launch welcome interceptor before typing the slash token.
    let startup = plan(
        &extension,
        "slash_command",
        json!({ "name": "usage", "dismiss_welcome": true }),
    );
    assert_eq!(
        startup.actions,
        vec![
            Action::Key(Key::Enter),
            Action::StreamText(StreamText {
                text: "/usage".to_string(),
                chunk_chars: None,
                delay_ms: None,
            }),
            Action::Key(Key::Enter),
        ]
    );

    // Empty / missing command — no-op (no actions, no intent change),
    // analogous to the empty-send_prompt guard.
    let empty = plan(&extension, "slash_command", json!({ "command": "" }));
    assert!(empty.actions.is_empty());
    assert!(empty.last_intent.is_none());

    let missing = plan(&extension, "slash_command", json!({}));
    assert!(missing.actions.is_empty());
    assert!(missing.last_intent.is_none());
}

/// An empty `send_prompt` must NOT mark `last_intent = "prompt_submitted"`.
/// The two Enters are no-ops on an empty input box, so Claude stays at
/// idle and never renders the `✻ <Verb> for <duration>` completion
/// marker that's the mid-turn release signal — claiming an intent here
/// would lock the classifier in `thinking` indefinitely.
///
/// Equally important: an empty `send_prompt` must explicitly CLEAR any
/// previously-recorded intent. If a real turn completed and then the
/// caller submitted an empty prompt, leaving the stale
/// `prompt_submitted` intent would keep mid-turn / completed-turn
/// branches active against an idle screen. The plan signals "clear
/// the intent" by returning `last_intent = Some("")`; the host's
/// `apply_plan` treats the empty string as an explicit reset.
#[test]
fn send_prompt_refuses_empty_input_and_clears_any_stale_intent() {
    let extension = claude_plugin();

    let empty_string = plan(&extension, "send_prompt", json!({ "prompt": "" }));
    assert!(
        empty_string.actions.is_empty(),
        "empty prompt must emit zero actions, got {:?}",
        empty_string.actions
    );
    assert_eq!(
        empty_string.last_intent.as_deref(),
        Some(""),
        "empty prompt must return Some(\"\") to explicitly clear any stale intent recorded by a prior submission"
    );

    // Missing prompt field is the same edge case — the plugin defaults
    // it to "" and must take the same path.
    let missing_prompt = plan(&extension, "send_prompt", json!({}));
    assert!(missing_prompt.actions.is_empty());
    assert_eq!(missing_prompt.last_intent.as_deref(), Some(""));
}

/// `force_cancel` sends Escape twice in one plan. Real Claude Code
/// occasionally needs the second press to interrupt a tool call that's
/// already serializing an API request: the first Escape lands while
/// Claude is mid-call and gets honored only after the response returns.
/// This intent is the named escalation path so callers don't script
/// the second press themselves.
#[test]
fn force_cancel_plan_emits_two_escapes_and_marks_cancelling_intent() {
    let extension = claude_plugin();
    let plan = plan(&extension, "force_cancel", json!({}));

    assert_eq!(
        plan.actions,
        vec![Action::Key(Key::Escape), Action::Key(Key::Escape)]
    );
    assert_eq!(
        plan.last_intent.as_deref(),
        Some("cancelling"),
        "force_cancel must mark the cancelling hold-state just like cancel does"
    );
}

/// End-to-end check that empty `send_prompt` clears a previously-set
/// `last_intent` through `ExtensionHandle::apply_plan`. The plan
/// returns `Some("")` which the host treats as an explicit reset.
/// Without this, a no-op submission after a real turn would leave
/// the classifier reading screens against a stale `prompt_submitted`
/// intent — locking the mid-turn `thinking` branch on an idle screen.
#[test]
#[cfg(unix)]
fn empty_send_prompt_clears_stale_last_intent_via_handle() {
    use ptywright::session::Session;
    use ptywright::target::Target;

    let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "cat"]))
        .expect("spawn /bin/sh -lc cat for PTY round-trip");
    let extension = claude_plugin();
    let mut handle = ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);

    // Simulate a prior real submission having recorded the intent.
    handle.set_last_intent(Some("prompt_submitted".to_string()));
    assert_eq!(handle.last_intent(), Some("prompt_submitted"));

    // Empty `send_prompt` must clear the stale intent, not preserve it.
    handle
        .send("send_prompt", json!({ "prompt": "" }))
        .expect("send_prompt empty via ExtensionHandle");
    assert_eq!(
        handle.last_intent(),
        None,
        "empty send_prompt should explicitly reset last_intent so a stale `prompt_submitted` from a prior turn doesn't keep mid-turn branches active"
    );

    // Same for missing `prompt` field.
    handle.set_last_intent(Some("prompt_submitted".to_string()));
    handle
        .send("send_prompt", json!({}))
        .expect("send_prompt with no prompt field via ExtensionHandle");
    assert_eq!(handle.last_intent(), None);
}

#[test]
fn cancel_plan_emits_escape_and_marks_cancelling_intent() {
    // Claude Code 2.1.x captures Escape as the mid-turn interrupt key
    // (the active-work indicator literally renders "esc to interrupt").
    // Sending Ctrl-C here used to either be ignored mid-turn or trigger
    // Claude's idle exit-confirmation flow instead of cancelling the
    // current turn.
    let extension = claude_plugin();
    let plan = plan(&extension, "cancel", json!({}));

    assert_eq!(plan.actions, vec![Action::Key(Key::Escape)]);
    assert_eq!(plan.last_intent.as_deref(), Some("cancelling"));
}

#[test]
fn approve_plan_sends_single_enter_with_no_intent() {
    let extension = claude_plugin();
    let plan = plan(&extension, "approve", json!({}));

    assert_eq!(plan.actions, vec![Action::Key(Key::Enter)]);
    assert!(
        plan.last_intent.is_none(),
        "approve is non-mutating; last_intent must stay as-is"
    );
}

#[test]
fn deny_plan_sends_escape_with_no_intent() {
    let extension = claude_plugin();
    let plan = plan(&extension, "deny", json!({}));

    assert_eq!(plan.actions, vec![Action::Key(Key::Escape)]);
    assert!(plan.last_intent.is_none());
}

#[test]
fn choose_option_types_numeric_choice_and_submits() {
    let extension = claude_plugin();
    let plan = plan(&extension, "choose_option", json!({ "index": 3 }));

    assert_eq!(
        plan.actions,
        vec![Action::Text("3".to_string()), Action::Key(Key::Enter)],
    );
    assert!(
        plan.last_intent.is_none(),
        "choosing a dialog/list option must not mark a conversation turn"
    );
}

#[test]
fn choose_option_can_resolve_current_visible_label() {
    let extension = claude_plugin();
    let screen = "\
Claude Code

Bash command
  cargo test --locked --features _test-fixtures

Do you want to proceed?
❯ 1. Yes
  2. Yes, and don't ask again for cargo test
  3. No
";

    let state = classify_state(&extension, screen, 42, None, None);
    assert_eq!(state.state, "waiting_for_permission");

    let plan = plan(
        &extension,
        "choose_option",
        json!({ "option": "don't ask again" }),
    );
    assert_eq!(
        plan.actions,
        vec![Action::Text("2".to_string()), Action::Key(Key::Enter)],
    );
}

#[test]
fn choose_option_prefers_exact_label_before_substring_label() {
    let extension = claude_plugin();
    let screen = "\
Claude Code

Do you want to proceed?
❯ 1. Yes, proceed once
  2. Yes
  3. No
";

    let state = classify_state(&extension, screen, 43, None, None);
    assert_eq!(state.state, "waiting_for_permission");

    let plan = plan(&extension, "choose_option", json!({ "option": "yes" }));
    assert_eq!(
        plan.actions,
        vec![Action::Text("2".to_string()), Action::Key(Key::Enter)],
    );
}

#[test]
fn choose_option_rejects_out_of_range_index_when_options_are_known() {
    let extension = claude_plugin();
    let screen = "\
Claude Code

Do you want to proceed?
❯ 1. Yes
  2. No
";

    let state = classify_state(&extension, screen, 44, None, None);
    assert_eq!(state.state, "waiting_for_permission");

    let err = extension
        .plan("choose_option", &json!({ "index": 99 }))
        .expect_err("out-of-range option should be rejected when current options are known");
    assert!(
        err.to_string().contains("outside the current option range"),
        "unexpected error: {err}"
    );
}

#[test]
fn choose_option_accepts_model_picker_dialog_id() {
    let extension = claude_plugin();
    let screen = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/claude-code/fixtures/model_picker.txt"),
    )
    .expect("read model picker fixture");

    let state = classify_state(&extension, &screen, 45, None, None);
    assert_eq!(state.state, "waiting_for_model_select");
    let metadata = state.metadata.expect("model picker should expose metadata");
    let dialog_id = metadata
        .get("dialog_id")
        .and_then(|value| value.as_str())
        .expect("model picker should expose dialog_id");

    let plan = plan(
        &extension,
        "choose_option",
        json!({ "option": "opus", "dialog_id": dialog_id }),
    );
    assert_eq!(
        plan.actions,
        vec![Action::Text("2".to_string()), Action::Key(Key::Enter)],
    );

    let err = extension
        .plan(
            "choose_option",
            &json!({ "option": "opus", "dialog_id": "deadbeef" }),
        )
        .expect_err("stale model picker dialog_id must be rejected");
    assert!(
        err.to_string().contains("stale_dialog"),
        "unexpected error: {err}"
    );
}

#[test]
fn approve_trust_types_numeric_option_one() {
    // The workspace-trust dialog requires typing "1" before Enter; a bare
    // Enter does not accept option 1 in the Claude Code TUI. Lock the
    // action sequence so a future Lua edit cannot silently regress it.
    let extension = claude_plugin();
    let plan = plan(&extension, "approve_trust", json!({}));

    assert_eq!(
        plan.actions,
        vec![Action::Text("1".to_string()), Action::Key(Key::Enter)],
    );
}

#[test]
fn deny_trust_types_numeric_option_two() {
    let extension = claude_plugin();
    let plan = plan(&extension, "deny_trust", json!({}));

    assert_eq!(
        plan.actions,
        vec![Action::Text("2".to_string()), Action::Key(Key::Enter)],
    );
}

#[test]
fn key_intent_routes_named_keys_to_action_key() {
    // The generic `key` intent is the entry point the REPL DSL's
    // `send.key("…")` calls into. Named keys (and hyphenated control
    // aliases like `ctrl-c`) must serialise to the matching
    // `Action::Key(...)` variant.
    let extension = claude_plugin();

    let enter_plan = plan(&extension, "key", json!({ "key": "enter" }));
    assert_eq!(enter_plan.actions, vec![Action::Key(Key::Enter)]);
    assert!(enter_plan.last_intent.is_none());

    let ctrl_c_plan = plan(&extension, "key", json!({ "key": "ctrl-c" }));
    assert_eq!(ctrl_c_plan.actions, vec![Action::Key(Key::CtrlC)]);

    let ctrl_d_plan = plan(&extension, "key", json!({ "key": "ctrl_d" }));
    assert_eq!(ctrl_d_plan.actions, vec![Action::Key(Key::CtrlD)]);
}

#[test]
fn model_effort_intents_send_arrow_keys_without_intent() {
    let extension = claude_plugin();

    let left = plan(&extension, "model_effort_left", json!({}));
    assert_eq!(left.actions, vec![Action::Key(Key::Left)]);
    assert!(left.last_intent.is_none());

    let right = plan(&extension, "model_effort_right", json!({}));
    assert_eq!(right.actions, vec![Action::Key(Key::Right)]);
    assert!(right.last_intent.is_none());
}

#[test]
fn key_intent_routes_expanded_special_keys() {
    // The expanded key surface — Shift+Tab, the navigation cluster, the
    // function keys, and the broader ctrl-letter combos — must all flow
    // through `M.key` to `action.key` rather than falling through to
    // `action.text`. Hyphen and underscore are both accepted so callers
    // can write `shift-tab` or `shift_tab` interchangeably.
    let extension = claude_plugin();

    let cases = [
        ("shift-tab", Key::ShiftTab),
        ("shift_tab", Key::ShiftTab),
        ("home", Key::Home),
        ("end", Key::End),
        ("page-up", Key::PageUp),
        ("page_down", Key::PageDown),
        ("insert", Key::Insert),
        ("delete", Key::Delete),
        ("space", Key::Space),
        ("ctrl-r", Key::CtrlR),
        ("ctrl-u", Key::CtrlU),
        ("ctrl-w", Key::CtrlW),
        ("ctrl-z", Key::CtrlZ),
        ("f1", Key::F1),
        ("f5", Key::F5),
        ("f12", Key::F12),
    ];

    for (input, expected) in cases {
        let plan = plan(&extension, "key", json!({ "key": input }));
        assert_eq!(
            plan.actions,
            vec![Action::Key(expected.clone())],
            "key alias `{input}` did not route to Action::Key({expected:?})"
        );
        assert!(
            plan.last_intent.is_none(),
            "the generic `key` intent must not mutate last_intent"
        );
    }
}

/// EVERY variant of `Action::Key` (from `src/action.rs`) must be
/// represented in the Lua plugin's `KEY_ALIASES` table so the
/// generic `key` intent routes to `action.key(...)` rather than
/// silently falling through to `action.text`. The comment in
/// `plugins/claude-code/main.lua` promises this contract; this test
/// enforces it for the variants it lists.
///
/// **Reminder, NOT a safety net.** The variants below are hand-listed.
/// Adding a new variant to `Key` does NOT automatically fail this
/// test — maintainers must also add the new variant to the slice
/// below AND to Lua's `KEY_ALIASES`. The test catches drift only for
/// variants someone remembered to enumerate here. (`Action::Key`
/// doesn't implement `IntoEnumIter`, so there's no compile-time way
/// to derive the list.)
#[test]
fn key_intent_covers_every_rust_key_variant() {
    let extension = claude_plugin();

    // Hand-listed reminder of every `Key` variant currently shipping.
    // The serde encoding is `rename_all = "snake_case"`, so every
    // variant maps to the corresponding snake_case alias the Lua side
    // accepts.
    let variants: &[Key] = &[
        // Submission / line editing
        Key::Enter,
        Key::Escape,
        Key::Tab,
        Key::ShiftTab,
        Key::Backspace,
        Key::Delete,
        Key::Space,
        // Arrows
        Key::Up,
        Key::Down,
        Key::Left,
        Key::Right,
        // Navigation cluster
        Key::Home,
        Key::End,
        Key::PageUp,
        Key::PageDown,
        Key::Insert,
        // Ctrl combos. Note that `ctrl_h` / `ctrl_i` / `ctrl_j` / `ctrl_m`
        // are NOT defined as variants in `Key` — those control codes
        // are intentionally represented by the semantic aliases
        // (`backspace` / `tab` / `enter`) so transcripts stay readable
        // and the alias table doesn't carry two equivalent names for
        // the same wire byte.
        Key::CtrlA,
        Key::CtrlB,
        Key::CtrlC,
        Key::CtrlD,
        Key::CtrlE,
        Key::CtrlF,
        Key::CtrlG,
        Key::CtrlK,
        Key::CtrlL,
        Key::CtrlN,
        Key::CtrlO,
        Key::CtrlP,
        Key::CtrlQ,
        Key::CtrlR,
        Key::CtrlS,
        Key::CtrlT,
        Key::CtrlU,
        Key::CtrlV,
        Key::CtrlW,
        Key::CtrlX,
        Key::CtrlY,
        Key::CtrlZ,
        // Function keys
        Key::F1,
        Key::F2,
        Key::F3,
        Key::F4,
        Key::F5,
        Key::F6,
        Key::F7,
        Key::F8,
        Key::F9,
        Key::F10,
        Key::F11,
        Key::F12,
    ];

    for variant in variants {
        // Round-trip through serde to get the wire-form alias rather
        // than re-implementing snake_case here; if the rename_all
        // attribute on Key ever changes, this test follows it.
        let alias = serde_json::to_value(variant)
            .expect("Key serializes to JSON")
            .as_str()
            .expect("Key encodes as a string")
            .to_string();
        let plan = plan(&extension, "key", json!({ "key": alias.clone() }));
        assert_eq!(
            plan.actions,
            vec![Action::Key(variant.clone())],
            "Key::{variant:?} (snake_case alias `{alias}`) does not route through the Lua KEY_ALIASES table — \
             add it to `plugins/claude-code/main.lua::KEY_ALIASES` so the generic `key` intent stays in sync with the Rust enum"
        );
    }
}

#[test]
fn key_intent_falls_through_to_text_for_unrecognised_input() {
    // Single chars like "y" / "n" / "1" are sent through `action.text`
    // so REPL callers can use `send.key("y")` instead of
    // `send.text("y")` interchangeably for quick acknowledgements.
    let extension = claude_plugin();
    let y_plan = plan(&extension, "key", json!({ "key": "y" }));
    assert_eq!(y_plan.actions, vec![Action::Text("y".to_string())]);

    let one_plan = plan(&extension, "key", json!({ "key": "1" }));
    assert_eq!(one_plan.actions, vec![Action::Text("1".to_string())]);
}

#[test]
fn dismiss_welcome_sends_single_enter_without_intent() {
    // The first-launch welcome panel traps Enter; the plugin exposes
    // `dismiss_welcome` as a single-Enter action so callers don't have to
    // drop down to session.input to clear it.
    let extension = claude_plugin();
    let plan = plan(&extension, "dismiss_welcome", json!({}));

    assert_eq!(plan.actions, vec![Action::Key(Key::Enter)]);
    assert!(
        plan.last_intent.is_none(),
        "dismiss_welcome is non-mutating; last_intent must stay as-is"
    );
}

#[test]
fn wait_turn_matcher_includes_v2_trust_dialog_anchors() {
    // The classifier recognises Claude Code 2.1's `Accessing workspace` /
    // `Yes, I trust this folder` dialog; the wait matcher must wake on the
    // same dialog or callers waiting after `adapter.start` against a fresh
    // untrusted directory will time out. Lock the anchors in here so a
    // future Lua edit can't silently drop them.
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_turn_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
    );

    let Matcher::All(matchers) = matcher else {
        panic!("expected wait matcher to require all conditions");
    };
    let Some(Matcher::Any(boundary_matchers)) = matchers
        .iter()
        .find(|matcher| matches!(matcher, Matcher::Any(_)))
    else {
        panic!("expected wait matcher to include boundary alternatives");
    };
    assert!(
        boundary_matchers
            .iter()
            .any(|matcher| matcher == &Matcher::ContainsText("Total cost:".to_string()))
    );
    assert!(
        boundary_matchers
            .iter()
            .any(|matcher| matcher == &Matcher::ContainsText("Ready to code?".to_string()))
    );
    assert!(
        boundary_matchers
            .iter()
            .any(|matcher| matcher == &Matcher::ContainsText("Welcome back".to_string())),
        "wait matcher missing welcome-screen anchor; boundary matchers were {boundary_matchers:?}",
    );
    assert!(
        boundary_matchers
            .iter()
            .any(|matcher| matcher == &Matcher::ContainsText("Select model".to_string()))
    );
    assert!(boundary_matchers.iter().any(|matcher| matcher
        == &Matcher::ContainsText("Save and close editor to continue".to_string())));
    assert!(
        boundary_matchers
            .iter()
            .any(|matcher| matcher == &Matcher::ContainsText("Accessing workspace".to_string())),
        "wait matcher missing v2 trust dialog anchor; boundary matchers were {boundary_matchers:?}",
    );
    assert!(
        boundary_matchers.iter().any(
            |matcher| matcher == &Matcher::ContainsText("Yes, I trust this folder".to_string())
        ),
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
fn wait_cancel_settled_matcher_returns_screen_stable_threshold() {
    // After `cancel` the classifier holds in `cancelling` until the screen
    // has been stable for `completed_turn_stable_ms`. A caller who wants to
    // wait for the post-cancel transition has no first-class way to express
    // that without re-implementing the threshold themselves — surface it as
    // a named matcher so the plugin stays the source of truth for what
    // "cancel landed" means.
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_cancel_settled_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
    );

    assert!(matches!(
        matcher,
        Matcher::ScreenStable {
            min_ms: COMPLETED_TURN_STABLE_MS
        }
    ));

    let snapshot = ScreenSnapshot {
        size: TerminalSize::new(3, 20),
        cursor: CursorState {
            row: 1,
            col: 2,
            visible: true,
        },
        sequence: 1,
        plain_text: "post-cancel prompt".to_string(),
        cells: Vec::new(),
        alternate_screen: false,
        application_cursor: false,
        application_keypad: false,
        title: None,
    };
    assert!(
        !matcher.is_match_with_context(
            &snapshot,
            "",
            MatcherContext {
                stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS - 1),
                process_exited: false,
            },
        ),
        "matcher must not fire before the stable threshold elapses"
    );
    assert!(matcher.is_match_with_context(
        &snapshot,
        "",
        MatcherContext {
            stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
            process_exited: false,
        },
    ));
}

#[test]
fn wait_turn_matcher_matches_idle_prompt_glyph_when_screen_settles() {
    // The prompt anchor in wait_turn_matcher is now paired with
    // the tea-verb completion marker — both must appear on the screen
    // for the prompt branch to fire. This mirrors the
    // classifier's `completed_turn` gate exactly so `adapter.wait`
    // can't return before the classifier would.
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_turn_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
    );
    let snapshot = ScreenSnapshot {
        size: TerminalSize::new(4, 30),
        cursor: CursorState {
            row: 2,
            col: 2,
            visible: true,
        },
        sequence: 1,
        plain_text: "work complete\n✻ Brewed for 0.4s\n > \r\n".to_string(),
        cells: Vec::new(),
        alternate_screen: false,
        application_cursor: false,
        application_keypad: false,
        title: None,
    };

    assert!(matcher.is_match_with_context(
        &snapshot,
        "",
        MatcherContext {
            stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
            process_exited: false,
        },
    ));
}

#[test]
fn wait_turn_matcher_matches_prompt_with_suggested_text_after_completion_marker() {
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_turn_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
    );
    let snapshot = ScreenSnapshot {
        size: TerminalSize::new(4, 60),
        cursor: CursorState {
            row: 2,
            col: 2,
            visible: true,
        },
        sequence: 1,
        plain_text: "work complete\n✻ Cooked for 48s\n❯\u{00a0}run the tests\r\n".to_string(),
        cells: Vec::new(),
        alternate_screen: false,
        application_cursor: false,
        application_keypad: false,
        title: None,
    };

    assert!(matcher.is_match_with_context(
        &snapshot,
        "",
        MatcherContext {
            stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
            process_exited: false,
        },
    ));
}

#[test]
fn wait_turn_matcher_does_not_fire_on_preamble_without_completion_marker() {
    // Regression for the bug Copilot called out: the prompt
    // anchor used to wake `adapter.wait` on its own, which fired on
    // preamble-before-tool-use screens (answer-bullet line plus an
    // empty `❯` for a single frame while the next spinner was between
    // repaints). Without the tea-verb marker, the wait must hold and
    // let the next spinner frame re-trigger the active-work state
    // instead of returning prematurely with `waiting_for_user_input`.
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_turn_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
    );
    let snapshot = ScreenSnapshot {
        size: TerminalSize::new(4, 60),
        cursor: CursorState {
            row: 2,
            col: 2,
            visible: true,
        },
        sequence: 1,
        plain_text: "⏺ I'll explore the project structure and read the\nkey files.\n > \r\n"
            .to_string(),
        cells: Vec::new(),
        alternate_screen: false,
        application_cursor: false,
        application_keypad: false,
        title: None,
    };

    assert!(
        !matcher.is_match_with_context(
            &snapshot,
            "",
            MatcherContext {
                stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
                process_exited: false,
            },
        ),
        "wait_turn_matcher must not fire on a preamble screen lacking the ✻ completion marker"
    );
}

#[test]
fn classifier_holds_cancelling_until_screen_settles() {
    // Right after `cancel` is sent the PTY needs a moment to render the
    // post-interrupt state. If the classifier just falls through to its
    // ordinary branches during that window it reports
    // `waiting_for_user_input`, leaving callers with no signal that the
    // cancel actually landed. Hold `cancelling` until the screen has been
    // stable for `completed_turn_stable_ms` so polling drivers can observe
    // the transition.
    let extension = claude_plugin();
    let mid_cancel_screen = "❯ count to 10\n\n(interrupting…)\n\n────────\n❯\n";
    let state = classify_state(
        &extension,
        mid_cancel_screen,
        42,
        Some("cancelling"),
        Some(50),
    );

    assert_eq!(state.state, "cancelling");
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
    // actually shows. For an idle prompt that means `waiting_for_user_input`;
    // this lets the polling driver see cancel -> cancelling ->
    // waiting_for_user_input as a clean sequence rather than getting stuck
    // reporting `cancelling` forever.
    let extension = claude_plugin();
    let post_cancel_idle = "❯ count to 10\n\n────────\n❯\n";
    let state = classify_state(
        &extension,
        post_cancel_idle,
        43,
        Some("cancelling"),
        Some(COMPLETED_TURN_STABLE_MS + 100),
    );

    assert_ne!(
        state.state, "cancelling",
        "cancelling must release once the screen settles; got evidence {}",
        state.evidence,
    );
}

#[test]
fn classifier_detects_completed_turn_after_prompt_submission() {
    let extension = claude_plugin();
    let state = classify_state(
        &extension,
        "work completed\n\n✻ Brewed for 0.3s\n\n>",
        6,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );

    assert_eq!(state.state, "completed_turn");
    assert_eq!(
        state.evidence,
        "stable input prompt after prompt submission"
    );
}

#[test]
fn classifier_completed_turn_paths() {
    // The classifier has two completion paths:
    //   (a) Stable path (confidence ~0.78): fires when adapter.wait
    //       supplies stable_ms >= completed_turn_stable_ms. The matcher
    //       has already proven the screen settled, so we trust it.
    //   (b) Poll path (confidence ~0.7): fires from adapter.state when
    //       the answer bullet `⏺` + the tea-verb completion marker
    //       `✻ <Verb> for <duration>` + an empty input prompt line are
    //       all visible. The marker is the TUI's own end-of-turn signal —
    //       it never appears between tool calls or during preambles, so
    //       requiring it eliminates the "preamble-before-tool" false
    //       positive (bullet + empty prompt visible for a frame while
    //       the spinner hasn't repainted yet).
    //
    // Two mid-stream cases must NOT fire completed_turn — either would
    // terminate the stream before the rest of the answer arrives.
    let extension = claude_plugin();

    let completed_screen = "⏺ work completed\n\n✻ Brewed for 0.3s\n\n>";

    // (a) Stable path — completion screen + stable_ms supplied.
    let stable = classify_state(
        &extension,
        completed_screen,
        6,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );
    assert_eq!(stable.state, "completed_turn");
    assert_eq!(
        stable.evidence,
        "stable input prompt after prompt submission"
    );

    // (b) Poll path — completion screen, no stable_ms.
    let poll = classify_state(
        &extension,
        completed_screen,
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(poll.state, "completed_turn");
    assert_eq!(
        poll.evidence,
        "completion marker plus input prompt visible without active work"
    );
    assert!(
        poll.confidence < stable.confidence,
        "poll-path confidence ({}) should be below stable-path ({})",
        poll.confidence,
        stable.confidence
    );

    // (c) Answer bullet visible but no empty prompt yet — mid-stream.
    // Must NOT fire completed_turn, or the script terminates early.
    let mid_stream = classify_state(
        &extension,
        "⏺ partial answer being written",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_ne!(
        mid_stream.state, "completed_turn",
        "mid-stream (answer bullet but no empty prompt) must not fire completed_turn"
    );

    // (d) Preamble-before-tool-use — answer bullet + empty prompt visible
    // but NO completion marker yet (Claude rendered a preamble line and
    // is about to start a tool call; the spinner happens to be between
    // repaints this frame). Must NOT fire completed_turn — that was the
    // live regression from the claude-stream demo.
    let preamble = classify_state(
        &extension,
        "⏺ I'll explore the project structure and read the key files.\n\n>",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_ne!(
        preamble.state, "completed_turn",
        "preamble (bullet + empty prompt but no ✻ marker) must not fire completed_turn"
    );

    // (e) Same regression with Claude's tool-progress row visible. This is
    // the exact shape observed from claude-stream before it exited early:
    // a preamble bullet, "Reading 1 file...", and an empty prompt row, but
    // still no TUI completion marker.
    let preamble_with_tool_progress = classify_state(
        &extension,
        "⏺ I'll read through the key files in this project to give you a comprehensive summary.\n  Reading 1 file... (ctrl+o to expand)\n\n>",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_ne!(
        preamble_with_tool_progress.state, "completed_turn",
        "preamble with tool progress but no ✻ marker must not fire completed_turn"
    );

    // (f) Claude Code can render suggested follow-up text after the
    // prompt glyph once the turn is over. The completion marker is still
    // the durable boundary; the prompt row does not have to be empty.
    let completed_with_suggestion = classify_state(
        &extension,
        "⏺ work completed\n\n✻ Cooked for 48s\n\n❯\u{00a0}run the tests",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        completed_with_suggestion.state, "completed_turn",
        "completed screen with suggested prompt text must still fire completed_turn"
    );

    // (g) Long answers can push the original answer bullet out of the
    // visible screen before the completion marker and prompt row render.
    // The marker plus prompt is the durable end-of-turn signal.
    let completed_after_scroll = classify_state(
        &extension,
        "Current State\n- Stable: Generic core abstractions\n- Platform: macOS, Linux\n\n✻ Brewed for 36s\n❯\u{00a0}what's this branch about",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        completed_after_scroll.state, "completed_turn",
        "completed screen must not require the answer bullet to remain visible"
    );

    // (h) Live Claude Code footer notices below the prompt are trailing
    // chrome, not post-marker content. Claudette saw this exact shape:
    // the answer and completion marker were visible, but rotating footer
    // notices (`/mcp`, `/chrome`) kept the classifier stuck in `thinking`.
    let completed_with_live_footer_notice = classify_state(
        &extension,
        " ▐▛███▜▌   Claude Code v2.1.150\n\
▝▜█████▛▘  Sonnet 4.6 · Claude Max\n\
  ▘▘ ▝▝    ~/.claudette/workspaces/claudex/brazen-cedar\n\n\
❯ ping\n\n\
⏺ pong\n\n\
✻ Baked for 4s\n\n\
────────────────────────────────────────────────────────────────────────────────\n\
❯\u{00a0}\n\
────────────────────────────────────────────────────────────────────────────────\n\
  jamesbrink @ halcyon workspaces/claudex/brazen-cedar  james-brink/project-i…\n\
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents\n\
                                       2 claude.ai connectors need auth · /mcp",
        54,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        completed_with_live_footer_notice.state, "completed_turn",
        "footer notices below a valid completion marker must not keep the turn in thinking"
    );

    let completed_with_chrome_footer_notice = classify_state(
        &extension,
        "❯ ping\n\n⏺ pong\n\n✻ Baked for 4s\n\n❯\u{00a0}\n\nClaude in Chrome enabled · /chrome",
        57,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        completed_with_chrome_footer_notice.state, "completed_turn",
        "slash-command footer notices such as /chrome must be accepted after the marker"
    );

    // (i) Premature completed_turn regression — a screen where a
    // `✻ <Verb> for <N>` line is present (from some parser-captured
    // intermediate render or a transient Claude rendering) but tool
    // progress is STILL on screen below it must not fire
    // completed_turn. The structural check requires nothing meaningful
    // after the marker. This was the live bug producing 37-char-body
    // exits from claude-stream when Claude was actually still reading
    // files. Active-work indicator is also off here (between spinner
    // repaints) so the `(ctrl+o to expand)` anchor is what holds.
    let mid_turn_with_stray_marker = classify_state(
        &extension,
        "⏺ I'll explore the project structure first, then read the key files.\n\n✻ Brewed for 4s\n\n⏺ Reading 2 files… (ctrl+o to expand)",
        7,
        Some("prompt_submitted"),
        None,
    );
    assert_ne!(
        mid_turn_with_stray_marker.state, "completed_turn",
        "stray mid-turn marker followed by tool-progress content must NOT fire completed_turn"
    );

    // (j) The `(ctrl+o to expand)` hint is itself a mid-turn signal
    // (Claude Code's collapsible tool-progress rows render it). With
    // NO marker but tool progress visible, classifier must NOT fire
    // completed_turn — even though `⏺` isn't a spinner glyph and the
    // active-work indicator's spinner-line path won't fire either.
    let tool_progress_alone = classify_state(
        &extension,
        "⏺ I'll explore the project structure.\n\n⏺ Reading 1 file… (ctrl+o to expand)",
        5,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        tool_progress_alone.state, "thinking",
        "ctrl+o-to-expand tool-progress row is an active-work signal even without spinner glyph; state must be `thinking`"
    );
    assert_eq!(
        tool_progress_alone.evidence, "active work indicator detected",
        "active-work branch must own the classification, not the mid-turn fallback"
    );
}

/// When a turn is in flight (`last_intent == "prompt_submitted"`) and no
/// completion marker is on screen yet, the classifier must return
/// `thinking` regardless of whether the active-work indicator is visible
/// on this particular frame. Without this branch, the classifier
/// oscillates between `thinking` (spinner glyph captured) and
/// `waiting_for_user_input` (between spinner repaints — the submitted
/// prompt is still visible so `has_input_prompt` keeps returning true)
/// on every polling tick, which makes downstream consumers see spurious
/// state churn. The fix is timing-independent: the completion marker
/// (`✻ <Verb> for <duration>`) is the TUI's structural end-of-turn
/// signal, so its absence is the durable mid-turn signal.
#[test]
fn classifier_returns_thinking_mid_turn_even_between_spinner_frames() {
    let extension = claude_plugin();

    // The submitted prompt is still visible on screen, and the spinner
    // happens to be between repaints this tick. Pre-fix, this classified
    // as `waiting_for_user_input`. Post-fix, `thinking`.
    let between_spinner_frames = classify_state(
        &extension,
        "❯ Read all files in this project and summarize it.\n\n⏺ I'll explore the project structure.\n\n>",
        5,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        between_spinner_frames.state, "thinking",
        "mid-turn frames without the completion marker must stay on `thinking` instead of flipping to `waiting_for_user_input`"
    );
    assert_eq!(
        between_spinner_frames.evidence,
        "turn in flight; no accepted completion marker on screen"
    );

    // The spinner-visible frame must still resolve via the higher-confidence
    // active-work branch, not the mid-turn fallback.
    let spinner_visible = classify_state(
        &extension,
        "❯ Read all files in this project and summarize it.\n\n✶ Razzle-dazzling…\n\n>",
        5,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(spinner_visible.state, "thinking");
    assert_eq!(spinner_visible.evidence, "active work indicator detected");
    assert!(
        spinner_visible.confidence > between_spinner_frames.confidence,
        "active-work branch should win on confidence when the spinner IS visible"
    );

    // Once the completion marker lands, the mid-turn branch must release
    // so completed_turn can fire.
    let post_completion = classify_state(
        &extension,
        "❯ Read all files in this project and summarize it.\n\n⏺ Done.\n\n✻ Brewed for 5s\n\n>",
        5,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(
        post_completion.state, "completed_turn",
        "mid-turn branch must release once the completion marker is on screen"
    );
}

#[test]
fn classifier_prefers_active_work_over_prompt_glyph() {
    let extension = claude_plugin();
    let state = classify_state(
        &extension,
        "Thinking... Esc to interrupt\n>",
        7,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );

    assert_eq!(state.state, "thinking");
    assert_eq!(state.evidence, "active work indicator detected");
}

#[test]
fn classifier_detects_usage_screen_as_completed_turn() {
    let extension = claude_plugin();
    let fixture = include_str!("../plugins/claude-code/fixtures/usage.txt");
    let state = classify_state(
        &extension,
        fixture,
        8,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );

    assert_eq!(state.state, "completed_turn");
    assert_eq!(state.evidence, "stable usage screen detected");
    assert_eq!(
        state
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.pointer("/usage/limits/current session/percent_used"))
            .and_then(serde_json::Value::as_i64),
        Some(2)
    );
    assert_eq!(
        state
            .metadata
            .as_ref()
            .and_then(|metadata| {
                metadata.pointer("/usage/limits/current week (all models)/percent_used")
            })
            .and_then(serde_json::Value::as_i64),
        Some(98)
    );
}

#[test]
fn approve_with_dialog_id_succeeds_after_matching_classify() {
    // Walk the full classifier → approve dispatch so the module-level
    // `_current_dialog_id` is set by classify before the intent reads
    // it back. The fixture under plugins/claude-code/fixtures/permission.txt
    // hashes to `69b9f3d0` (locked in permission.expected.json), so
    // passing that id through approve must succeed.
    let extension = claude_plugin();
    let screen = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/claude-code/fixtures/permission.txt"),
    )
    .expect("read permission fixture");
    let _ = classify_state(&extension, &screen, 1, None, Some(COMPLETED_TURN_STABLE_MS));
    let plan = plan(&extension, "approve", json!({ "dialog_id": "69b9f3d0" }));
    assert_eq!(plan.actions, vec![Action::Key(Key::Enter)]);
}

#[test]
fn approve_with_stale_dialog_id_returns_error() {
    // Same fixture; passing a mismatched dialog_id must surface as a
    // Lua error rather than silently pressing Enter on something else.
    let extension = claude_plugin();
    let screen = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/claude-code/fixtures/permission.txt"),
    )
    .expect("read permission fixture");
    let _ = classify_state(&extension, &screen, 1, None, Some(COMPLETED_TURN_STABLE_MS));
    let err = extension
        .plan("approve", &json!({ "dialog_id": "deadbeef" }))
        .expect_err("stale dialog_id must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("stale_dialog"),
        "error should explain the rejection: {msg}"
    );
}

#[test]
fn approve_with_dialog_id_when_no_dialog_active_is_rejected() {
    // Empty classifier run drops `_current_dialog_id`. Caller still
    // passes a dialog_id (perhaps from a previous frame); the plugin
    // must refuse.
    let extension = claude_plugin();
    let _ = classify_state(&extension, "no dialog here", 1, None, Some(0));
    let err = extension
        .plan("approve", &json!({ "dialog_id": "abc12345" }))
        .expect_err("dialog_id without active dialog must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("stale_dialog"), "got: {msg}");
}

#[test]
fn approve_without_dialog_id_remains_backwards_compatible() {
    // The id check is opt-in: callers that don't thread `dialog_id`
    // through still get the legacy "press Enter" behaviour so existing
    // consumers don't break.
    let extension = claude_plugin();
    let plan = plan(&extension, "approve", json!({}));
    assert_eq!(plan.actions, vec![Action::Key(Key::Enter)]);
}

#[test]
fn attach_file_bracketed_pastes_path_without_enter() {
    // Claude Code 2.1.x normalises drag-drop and `@<path>` references to
    // a bracketed-paste of the absolute path. The intent deliberately
    // does NOT append Enter so callers can compose a prompt around the
    // attachment before submitting.
    let extension = claude_plugin();
    let plan = plan(
        &extension,
        "attach_file",
        json!({ "path": "/tmp/diagram.png" }),
    );
    assert_eq!(
        plan.actions,
        vec![Action::BracketedPaste("/tmp/diagram.png".to_string())]
    );
    assert!(
        plan.last_intent.is_none(),
        "attach_file is non-mutating wrt turn state"
    );
}

#[test]
fn attach_file_rejects_empty_path() {
    // Empty path is an obvious caller bug — surface it as a clean
    // plugin error rather than silently bracket-pasting nothing.
    let extension = claude_plugin();
    let err = extension
        .plan("attach_file", &json!({ "path": "" }))
        .expect_err("empty path must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("path is required"),
        "error should explain the rejection: {msg}"
    );
}

#[test]
fn attach_file_rejects_path_with_embedded_newline() {
    // A newline in the path would partial-submit the paste and split
    // the attachment across rows. Plugin guards against it.
    let extension = claude_plugin();
    let err = extension
        .plan("attach_file", &json!({ "path": "/tmp/a\nb.png" }))
        .expect_err("newline in path must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("must not contain newlines"),
        "error should explain the rejection: {msg}"
    );
}

#[test]
#[cfg(unix)]
fn extension_subscribe_observes_state_changes_after_send() {
    use ptywright::ExtensionEvent;
    use ptywright::session::Session;
    use ptywright::target::Target;

    let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "cat"]))
        .expect("spawn /bin/sh -lc cat for PTY round-trip");
    let extension = claude_plugin();
    let mut handle = ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);

    let rx = handle.subscribe();
    handle
        .send("send_prompt", json!({ "prompt": "hello from lua" }))
        .expect("send_prompt via ExtensionHandle");
    handle
        .session()
        .wait_for(
            &ptywright::Matcher::TranscriptContains("hello from lua".to_string()),
            Duration::from_secs(2),
        )
        .expect("transcript should contain the bracketed-paste payload");

    let events: Vec<ExtensionEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExtensionEvent::StateChanged(_))),
        "expected at least one StateChanged event after send; got {events:?}"
    );
    let _ = handle.session().kill();
}

/// End-to-end smoke test: build an [`ExtensionHandle`] over a `/bin/sh cat`
/// stand-in and exercise every documented mutating intent (`send_prompt`,
/// `approve`, `deny`, `cancel`) through the generic [`ExtensionHandle::send`]
/// surface. Verifies that the Rust host hands intent strings to the Lua
/// plugin verbatim and that the resulting actions reach the underlying PTY.
#[test]
#[cfg(unix)]
fn extension_handle_dispatches_mutating_intents_against_live_session() {
    use ptywright::session::Session;
    use ptywright::target::Target;

    let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "cat"]))
        .expect("spawn /bin/sh -lc cat for PTY round-trip");
    let extension = claude_plugin();
    let mut handle = ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);

    handle
        .send("send_prompt", json!({ "prompt": "hello from lua" }))
        .expect("send_prompt via ExtensionHandle");
    handle
        .session()
        .wait_for(
            &Matcher::TranscriptContains("hello from lua".to_string()),
            Duration::from_secs(2),
        )
        .expect("transcript should contain the bracketed-paste payload");

    handle
        .send("approve", json!({}))
        .expect("approve via ExtensionHandle");
    handle
        .send("deny", json!({}))
        .expect("deny via ExtensionHandle");
    handle
        .send("cancel", json!({}))
        .expect("cancel via ExtensionHandle");

    let _ = handle.session().kill();
}

#[test]
fn classify_completed_turn_requests_turn_end_marker_and_surfaces_transcript_metadata() {
    // Auto-marking contract: when the classifier transitions to
    // `completed_turn` after a submitted prompt, the snapshot returns
    //   * `host_marks = [{label="turn_end"}]` — host will stamp it
    //   * `metadata.transcript = {turn_start, turn_end}` — using the
    //      caller-supplied `cursor` as the pre-computed `turn_end`
    //      value, so the same response carries the metadata.
    let extension = claude_plugin();
    let fixture = include_str!("../plugins/claude-code/fixtures/completed.txt");

    let mut markers = std::collections::BTreeMap::new();
    markers.insert("turn_start".to_string(), 100);
    let snapshot = classify_with_markers(
        &extension,
        fixture,
        7,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        &markers,
        500,
    );

    assert_eq!(snapshot.state, "completed_turn");
    assert_eq!(
        snapshot.host_marks,
        vec![ptywright::extension::HostMark {
            label: "turn_end".to_string()
        }],
        "first completed_turn classify after submission must request turn_end"
    );

    let metadata = snapshot
        .metadata
        .as_ref()
        .expect("metadata must be populated");
    let transcript_md = metadata
        .get("transcript")
        .expect("metadata.transcript must be populated");
    assert_eq!(transcript_md["turn_start"], 100);
    assert_eq!(transcript_md["turn_end"], 500);

    let turn = metadata
        .get("turn")
        .expect("metadata.turn must be populated for structured output");
    assert_eq!(
        turn["text"], "Done. The tests pass.",
        "completed turns should expose assistant text without Claude Code TUI chrome"
    );
}

#[test]
fn classify_completed_turn_prefers_full_transcript_for_structured_output() {
    let extension = claude_plugin();
    let screen = "final visible tail only\n\n✻ Done for 37s\n\n❯ ";
    let transcript = "\
❯ Explore this project and tell me about it

⏺ First section

This is the beginning that scrolled out of the viewport.

## What it does

- One
- Two

✻ Done for 37s

❯ ";

    let snapshot = classify_with_transcript(
        &extension,
        screen,
        transcript,
        99,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );

    assert_eq!(snapshot.state, "completed_turn");
    let text = snapshot
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.pointer("/turn/text"))
        .and_then(serde_json::Value::as_str)
        .expect("structured turn text");

    assert!(text.contains("First section"));
    assert!(text.contains("This is the beginning"));
    assert!(text.contains("## What it does"));
    assert!(!text.contains("final visible tail only"));
}

#[test]
fn classify_completed_turn_does_not_re_emit_turn_end_when_marker_already_present() {
    // Idempotency: once the host has applied turn_end and the marker
    // is visible in the classifier context, subsequent classifies
    // must NOT re-request the mark. They should still surface the
    // existing metadata.transcript so callers polling adapter.state
    // see consistent data.
    let extension = claude_plugin();
    let fixture = include_str!("../plugins/claude-code/fixtures/completed.txt");

    let mut markers = std::collections::BTreeMap::new();
    markers.insert("turn_start".to_string(), 100);
    markers.insert("turn_end".to_string(), 500);
    let snapshot = classify_with_markers(
        &extension,
        fixture,
        8,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        &markers,
        650,
    );

    assert_eq!(snapshot.state, "completed_turn");
    assert!(
        snapshot.host_marks.is_empty(),
        "turn_end already marked — must not re-emit host_marks"
    );

    let metadata = snapshot
        .metadata
        .as_ref()
        .expect("metadata must remain populated");
    let transcript_md = metadata
        .get("transcript")
        .expect("metadata.transcript must remain populated");
    assert_eq!(transcript_md["turn_start"], 100);
    assert_eq!(
        transcript_md["turn_end"], 500,
        "surfaced turn_end must come from markers, not cursor, once it exists"
    );
}

#[test]
fn classify_non_completed_state_does_not_request_turn_end_marker() {
    // Guard: the turn_end host_mark must only fire on completed_turn.
    // A thinking / dialog / cancelling classify under the same
    // last_intent must leave host_marks empty.
    let extension = claude_plugin();
    let fixture = include_str!("../plugins/claude-code/fixtures/thinking.txt");

    let mut markers = std::collections::BTreeMap::new();
    markers.insert("turn_start".to_string(), 100);
    let snapshot = classify_with_markers(
        &extension,
        fixture,
        9,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        &markers,
        500,
    );

    assert_ne!(
        snapshot.state, "completed_turn",
        "fixture should classify as a mid-turn state, not completion"
    );
    assert!(
        snapshot.host_marks.is_empty(),
        "non-completion state must not request turn_end mark; got {:?}",
        snapshot.host_marks
    );
}

#[test]
#[cfg(unix)]
fn extension_handle_applies_plan_driven_mark_transcript_action() {
    // End-to-end: when a plan contains `Action::MarkTranscript`, the
    // host's `apply_actions` path applies it against the underlying
    // session's transcript. The applied marker is observable via
    // `Session::transcript_marker`. This covers the plan-driven
    // marker path. The classifier-driven `host_marks` path is
    // covered separately by `classify_completed_turn_requests_turn_end_marker_*`
    // (which assert the snapshot's `host_marks` field) and by
    // `ExtensionHandle::apply_host_marks` consuming that field
    // immediately after classify returns (exercised on every send /
    // wait path through the existing fixture-driven tests).
    use ptywright::session::Session;
    use ptywright::target::Target;

    let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "cat"]))
        .expect("spawn /bin/sh -lc cat for PTY round-trip");
    let extension = claude_plugin();
    let mut handle = ExtensionHandle::start(Box::new(extension), session, COMPLETED_TURN_STABLE_MS);

    // send_prompt's plan contains a `MarkTranscript("turn_start")`
    // action — applying it stamps the marker on the live session.
    handle
        .send("send_prompt", json!({ "prompt": "hello there" }))
        .expect("send_prompt via ExtensionHandle");

    assert!(
        handle.session().transcript_marker("turn_start").is_some(),
        "send_prompt plan must have stamped turn_start via Action::MarkTranscript"
    );

    let _ = handle.session().kill();
}
