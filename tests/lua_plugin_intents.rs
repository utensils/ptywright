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
    // The plan emits THREE actions in this exact order:
    //   1. Enter  — dismiss any first-keypress interceptor (welcome
    //      panel, compact-launch view). On a clean empty input box
    //      Claude treats this as a no-op submit.
    //   2. BracketedPaste(prompt) — Claude Code v2.1+ requires the
    //      bracketed wrapper so the trailing Enter is not absorbed into
    //      the paste tokeniser on longer prompts.
    //   3. Enter — submit the now-populated input box.
    //
    // Without action #1, the bracketed paste's CSI-200~ open marker
    // gets consumed by Claude's first-keypress interceptor on a fresh
    // launch, the rest of the paste lands as input that's then
    // truncated, and the trailing Enter submits a partial prompt or
    // nothing at all. Locking the three-action sequence here so a
    // future plugin edit can't silently regress to the old two-action
    // form.
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
            Action::BracketedPaste("hello Claude".to_string()),
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
fn slash_command_pastes_token_and_presses_enter_without_intent() {
    let extension = claude_plugin();

    // Bare name — plugin adds the leading slash.
    let bare = plan(&extension, "slash_command", json!({ "command": "btw" }));
    assert_eq!(
        bare.actions,
        vec![
            Action::BracketedPaste("/btw".to_string()),
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
            Action::BracketedPaste("/btw".to_string()),
            Action::Key(Key::Enter),
        ]
    );

    // `name` is accepted as an alias for `command` so callers can use
    // either field idiomatically.
    let via_name = plan(&extension, "slash_command", json!({ "name": "usage" }));
    assert_eq!(
        via_name.actions,
        vec![
            Action::BracketedPaste("/usage".to_string()),
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

/// EVERY variant of `Action::Key` (from `src/action.rs`) must be
/// represented in the Lua plugin's `KEY_ALIASES` table so the
/// generic `key` intent routes to `action.key(...)` rather than
/// silently falling through to `action.text`. The comment in
/// `plugins/claude-code/main.lua` promises this contract; this test
/// enforces it.
///
/// When a new variant is added to the Rust enum, this test will
/// fail with a clear message naming the missing alias. The fix is
/// to add that snake_case name to `KEY_ALIASES`.
#[test]
fn key_intent_covers_every_rust_key_variant() {
    let extension = claude_plugin();

    // Hand-listed because `Action::Key` doesn't implement `IntoEnumIter`.
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

    // (h) Premature completed_turn regression — a screen where a
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

    // (i) The `(ctrl+o to expand)` hint is itself a mid-turn signal
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
        "turn in flight; no completion marker on screen"
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
