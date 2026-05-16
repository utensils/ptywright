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
use ptywright::{Action, MatcherContext};

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

    // Without stable_ms — adapter.state poll path. Falls into the new
    // lower-confidence completed_turn branch added for poll consumers.
    let st_poll = classify_state(&extension, screen, 7, Some("prompt_submitted"), None);
    assert_eq!(
        st_poll.state, "completed_turn",
        "welcome chrome must not downgrade to `starting` on state-poll path; got state={} evidence={}",
        st_poll.state, st_poll.evidence
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
fn send_prompt_plan_uses_bracketed_paste_and_sets_intent() {
    // Claude Code v2.1+ enables bracketed paste; the plan must use the
    // bracketed variant so the trailing Enter is not absorbed into the
    // paste tokeniser on longer prompts. Lock the action shape and the
    // `last_intent` contract in here — both are part of the plugin's
    // documented surface for plugin authors.
    let extension = claude_plugin();
    let plan = plan(
        &extension,
        "send_prompt",
        json!({ "prompt": "hello Claude" }),
    );

    assert_eq!(
        plan.actions,
        vec![
            Action::BracketedPaste("hello Claude".to_string()),
            Action::Key(Key::Enter),
        ]
    );
    assert_eq!(plan.last_intent.as_deref(), Some("prompt_submitted"));
}

#[test]
fn cancel_plan_emits_interrupt_and_marks_cancelling_intent() {
    let extension = claude_plugin();
    let plan = plan(&extension, "cancel", json!({}));

    assert_eq!(plan.actions, vec![Action::Interrupt]);
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
    let extension = claude_plugin();
    let matcher = wait_matcher(
        &extension,
        "wait_turn_matcher",
        json!({ "completed_turn_stable_ms": COMPLETED_TURN_STABLE_MS }),
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
        MatcherContext {
            stable_for: Duration::from_millis(COMPLETED_TURN_STABLE_MS),
            process_exited: false,
        },
    ));
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
        "work completed\n>",
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
fn classifier_distinguishes_stable_and_poll_paths_for_completed_turn() {
    // Two completed_turn branches exist for the post-prompt-submit case:
    //   • The high-confidence branch (~0.78) fires when the matcher
    //     supplies `stable_ms >= completed_turn_stable_ms` — used by
    //     `adapter.wait` and locked in by the `completed.txt` fixture.
    //   • A lower-confidence branch (~0.6) fires from state-poll callers
    //     (`adapter.state`) that don't have stability info but observe
    //     no active-work spinner. Without this branch, polling consumers
    //     would never see turn completion through state alone.
    //
    // Lock both paths so a future plugin edit can't collapse them or
    // re-introduce the previous conservative `waiting_for_user_input`
    // fallback that left polling consumers stuck mid-turn.
    let extension = claude_plugin();
    // `⏺` is Claude's answer-block bullet — the anchor the poll-path
    // completion check uses to gate against firing before the turn has
    // actually run. Without it (or before the answer arrives) state
    // polling falls back to `waiting_for_user_input`.
    let screen = "⏺ work completed\n>";

    // adapter.wait path: stable_ms passed.
    let stable = classify_state(
        &extension,
        screen,
        6,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );
    assert_eq!(stable.state, "completed_turn");
    assert_eq!(
        stable.evidence,
        "stable input prompt after prompt submission"
    );

    // adapter.state poll path: no stable_ms.
    let poll = classify_state(&extension, screen, 6, Some("prompt_submitted"), None);
    assert_eq!(poll.state, "completed_turn");
    assert_eq!(
        poll.evidence,
        "answer bullet visible after submission without active work"
    );
    assert!(
        poll.confidence < stable.confidence,
        "poll-path confidence ({}) should be below stable-path confidence ({})",
        poll.confidence,
        stable.confidence
    );

    // Without the `⏺` anchor the poll path should NOT fire — this is
    // the guard against "just submitted, spinner not rendered yet"
    // being mistaken for "turn complete".
    let no_bullet = classify_state(
        &extension,
        "work completed\n>",
        6,
        Some("prompt_submitted"),
        None,
    );
    assert_eq!(no_bullet.state, "waiting_for_user_input");
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
    let fixture = include_str!("fixtures/claude_code/usage.txt");
    let state = classify_state(
        &extension,
        fixture,
        8,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
    );

    assert_eq!(state.state, "completed_turn");
    assert_eq!(state.evidence, "stable usage screen detected");
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
