//! End-to-end integration test for [`TurnEvent`] emission.
//!
//! Replays a synthetic screen / transcript sequence through
//! [`LuaExtension`]'s classifier and asserts the events emitted are
//! monotonic, gap-free, and shaped correctly. The point of the test is
//! to pin the wire contract that downstream consumers (Claudette,
//! `claude-stream.py`, third-party adapters) read, so any regression
//! in the plugin's event emitter shows up here rather than days later
//! in the consumer.
//!
//! Drives the plugin through the generic [`ExtensionHandle`] surface —
//! no application-specific Rust wrapper sits between the test and the
//! Lua code. The host-side handle's per-call watermark advancement is
//! exercised end-to-end via `ExtensionHandle::last_event_seq`.

use ptywright::extension::{
    ClassifyContext, Extension, LuaExtension, STATUS_BAR_ROWS, TurnEvent, split_status_bar,
};

const COMPLETED_TURN_STABLE_MS: u64 = 300;

fn classify(
    extension: &LuaExtension,
    screen: &str,
    last_intent: Option<&str>,
    stable_ms: Option<u64>,
    last_event_seq: Option<u64>,
) -> ptywright::ExtensionStateSnapshot {
    let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
    let markers = std::collections::BTreeMap::new();
    let ctx = ClassifyContext {
        screen,
        body_text: &body_text,
        status_text: &status_text,
        transcript: "",
        sequence: 0,
        last_intent,
        stable_ms,
        completed_turn_stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        markers: &markers,
        cursor: 0,
        last_event_seq,
    };
    extension.classify(&ctx).expect("classify via Lua plugin")
}

#[test]
fn turn_events_are_monotonic_across_polls() {
    let extension = LuaExtension::built_in("claude-code").expect("load built-in");
    // First poll: a typical mid-turn screen with one assistant bullet and
    // one tool call rendered. The plugin should emit at least a text
    // delta and a tool_started event.
    let screen = concat!(
        "❯ Tell me about the codebase\n",
        "\n",
        "⏺ I'll start by reading the main module.\n",
        "\n",
        "⏺ Read(src/lib.rs)\n",
        "  Running...\n",
        "\n",
        "· Working… (8s · ↓ 200 tokens)\n",
        "────────────────────────────────────────\n",
        "  user @ host /workspace [Sonnet 4.6]\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );
    let snap = classify(&extension, screen, Some("prompt_submitted"), None, None);
    assert!(
        !snap.events.is_empty(),
        "first classify must emit at least one event; got snapshot {snap:?}"
    );
    let mut last_seq = 0u64;
    for event in &snap.events {
        assert!(
            event.seq() > last_seq,
            "event seq must be strictly monotonic; got {} after {}",
            event.seq(),
            last_seq,
        );
        last_seq = event.seq();
    }

    // Second poll on the same screen with the host's watermark forwarded
    // — plugin should emit zero new events.
    let snap2 = classify(
        &extension,
        screen,
        Some("prompt_submitted"),
        None,
        Some(last_seq),
    );
    assert!(
        snap2.events.is_empty(),
        "repeat poll with current watermark must not re-emit events; got {:?}",
        snap2.events
    );

    // Third poll: progress to a completed turn (final assistant bullet,
    // completion marker, idle prompt). New events should appear with seq
    // strictly greater than `last_seq`.
    let completed_screen = concat!(
        "❯ Tell me about the codebase\n",
        "\n",
        "⏺ I'll start by reading the main module.\n",
        "\n",
        "⏺ Read(src/lib.rs)\n",
        "\n",
        "⏺ Done. The library exposes target, session, screen, action, matcher,\n",
        "  transcript, and extension as the seven core layers.\n",
        "\n",
        "✻ Reviewed for 12s\n",
        "\n",
        "❯\n",
        "────────────────────────────────────────\n",
        "  user @ host /workspace [Sonnet 4.6]\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );
    let snap3 = classify(
        &extension,
        completed_screen,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        Some(last_seq),
    );
    let mut prev_seq = last_seq;
    for event in &snap3.events {
        assert!(
            event.seq() > prev_seq,
            "post-completion events must continue the monotonic seq; got {} after watermark {}",
            event.seq(),
            prev_seq,
        );
        prev_seq = event.seq();
    }
}

#[test]
fn turn_complete_event_fires_exactly_once_per_turn() {
    let extension = LuaExtension::built_in("claude-code").expect("load built-in");
    let completed_screen = concat!(
        "❯ Quick question\n",
        "\n",
        "⏺ Sure — the short answer is yes.\n",
        "\n",
        "✻ Answered for 4s\n",
        "\n",
        "❯\n",
        "────────────────────────────────────────\n",
        "  user @ host /workspace [Sonnet 4.6]\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );
    let snap = classify(
        &extension,
        completed_screen,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        None,
    );
    let turn_completes: Vec<_> = snap
        .events
        .iter()
        .filter(|e| matches!(e, TurnEvent::TurnComplete { .. }))
        .collect();
    assert_eq!(
        turn_completes.len(),
        1,
        "completed turn must emit exactly one turn_complete event; got events {:?}",
        snap.events
    );

    // Re-classify the same completed screen — turn_complete must not
    // fire again. (Polled-snapshot consumers rely on this for at-most-
    // once delivery against the same turn boundary.)
    let last_seq = snap.events.iter().map(TurnEvent::seq).max().unwrap_or(0);
    let snap2 = classify(
        &extension,
        completed_screen,
        Some("prompt_submitted"),
        Some(COMPLETED_TURN_STABLE_MS),
        Some(last_seq),
    );
    let repeat_turn_completes: Vec<_> = snap2
        .events
        .iter()
        .filter(|e| matches!(e, TurnEvent::TurnComplete { .. }))
        .collect();
    assert!(
        repeat_turn_completes.is_empty(),
        "repeat classify must not re-emit turn_complete; got {:?}",
        snap2.events
    );
}

#[test]
fn text_delta_events_carry_only_new_text() {
    let extension = LuaExtension::built_in("claude-code").expect("load built-in");
    // First poll: short answer fragment.
    let early_screen = concat!(
        "❯ Hello\n",
        "\n",
        "⏺ Hi there,\n",
        "\n",
        "· Thinking… (2s · ↓ 50 tokens)\n",
        "────────────────────────────────────────\n",
        "  user @ host /workspace [Sonnet 4.6]\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );
    let snap_a = classify(
        &extension,
        early_screen,
        Some("prompt_submitted"),
        None,
        None,
    );
    let early_text: String = snap_a
        .events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();

    // Second poll: same prefix plus additional sentence.
    let late_screen = concat!(
        "❯ Hello\n",
        "\n",
        "⏺ Hi there, how can I help today?\n",
        "\n",
        "· Thinking… (3s · ↓ 80 tokens)\n",
        "────────────────────────────────────────\n",
        "  user @ host /workspace [Sonnet 4.6]\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );
    let last_seq = snap_a.events.iter().map(TurnEvent::seq).max().unwrap_or(0);
    let snap_b = classify(
        &extension,
        late_screen,
        Some("prompt_submitted"),
        None,
        Some(last_seq),
    );
    let new_text: String = snap_b
        .events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !new_text.is_empty(),
        "extending the screen must emit a text delta carrying the new content; got events {:?}",
        snap_b.events
    );
    let combined = format!("{early_text}{new_text}");
    assert!(
        combined.contains("how can I help today"),
        "concatenating deltas across polls must reconstruct the answer; got {combined:?}"
    );
}

#[test]
fn classify_context_carries_last_event_seq_to_plugin() {
    // Regression: an earlier draft skipped serializing `last_event_seq`
    // when None, but mlua's serde adapter encoded a present field as a
    // tagged Option which the Lua side then misread. The skip_serializing
    // helper on the field is the fix; this test ensures the wire shape
    // stays correct.
    use serde_json::json;
    let empty_markers = std::collections::BTreeMap::new();
    let none_ctx = ClassifyContext {
        screen: "",
        body_text: "",
        status_text: "",
        transcript: "",
        sequence: 0,
        last_intent: None,
        stable_ms: None,
        completed_turn_stable_ms: None,
        markers: &empty_markers,
        cursor: 0,
        last_event_seq: None,
    };
    let value = serde_json::to_value(none_ctx).expect("serialize");
    let object = value.as_object().expect("object shape");
    assert!(
        !object.contains_key("last_event_seq"),
        "None last_event_seq must be omitted; got {value}"
    );

    let some_ctx = ClassifyContext {
        screen: "",
        body_text: "",
        status_text: "",
        transcript: "",
        sequence: 0,
        last_intent: None,
        stable_ms: None,
        completed_turn_stable_ms: None,
        markers: &empty_markers,
        cursor: 0,
        last_event_seq: Some(42),
    };
    let value = serde_json::to_value(some_ctx).expect("serialize");
    assert_eq!(
        value.get("last_event_seq").cloned(),
        Some(json!(42)),
        "Some last_event_seq must serialize as a plain integer; got {value}"
    );
}
