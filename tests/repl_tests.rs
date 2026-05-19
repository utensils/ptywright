//! Integration tests for the `repl` Cargo feature.
//!
//! These run only when the binary is built with `--features repl`. They
//! exercise the public surfaces of `ptywright::repl::*` against an
//! in-process JSON-RPC server, avoiding terminal-state mutation so they
//! can run inside CI without a TTY.

#![cfg(feature = "repl")]

use std::io::pipe;
use std::sync::Arc;
use std::time::Duration;

use ptywright::repl::Framing;
use ptywright::repl::command::{CmdOutcome, dispatch, parse};
use ptywright::repl::ctx::ReplCtx;
use ptywright::repl::transport::RpcClient;
use ptywright::rpc::{RpcServerState, serve_ndjson_with_state};
use serde_json::json;

fn in_process_client() -> (Arc<RpcClient>, std::thread::JoinHandle<()>) {
    let (c2s_r, c2s_w) = pipe().expect("pipe c→s");
    let (s2c_r, s2c_w) = pipe().expect("pipe s→c");
    let state = RpcServerState::new();
    let server = std::thread::spawn(move || {
        let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
    });
    let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
    (client, server)
}

#[test]
fn rpc_client_surfaces_capabilities_and_adapter_methods() {
    let (client, _server) = in_process_client();
    let capabilities = client
        .call("server.capabilities", json!({}), Duration::from_secs(5))
        .expect("server.capabilities");
    let methods = capabilities["methods"].as_array().expect("methods array");
    let names: Vec<&str> = methods.iter().filter_map(|m| m.as_str()).collect();
    for expected in [
        "adapter.list",
        "adapter.start",
        "adapter.resume",
        "adapter.send",
        "adapter.wait",
        "adapter.snapshot",
        "adapter.transcript",
        "adapter.inspect",
        "adapter.close",
        "server.set_notifications",
    ] {
        assert!(
            names.contains(&expected),
            "capabilities is missing `{expected}`; got {names:?}",
        );
    }

    let plugins = client
        .call("adapter.list", json!({}), Duration::from_secs(5))
        .expect("adapter.list");
    let plugins_array = plugins["plugins"].as_array().expect("plugins array");
    let has_claude = plugins_array.iter().any(|p| {
        p.get("name")
            .and_then(|n| n.as_str())
            .map(|n| n == "claude-code")
            .unwrap_or(false)
    });
    assert!(
        has_claude,
        "expected `claude-code` in adapter.list response"
    );
}

#[test]
fn command_dispatcher_drives_full_spawn_and_close_cycle() {
    let (client, _server) = in_process_client();
    let mut ctx = ReplCtx::new();

    // plugins() should return the built-in list.
    let outcome = dispatch(
        parse("plugins()").unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    )
    .expect("dispatch plugins");
    assert!(matches!(outcome, CmdOutcome::Json(_)));

    // The dispatch path that actually spawns a PTY is unix-only because
    // the claude-code plugin's default target points at /bin/sh on
    // Linux/macOS. On Windows the test stops at the previous assertion.
    #[cfg(unix)]
    {
        let outcome = dispatch(
            parse(r#"session.spawn("claude-code", program="/bin/sh")"#).unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("dispatch spawn");
        let CmdOutcome::Note { value, note } = outcome else {
            panic!("expected note outcome from spawn")
        };
        // The note must carry the spawn summary so the TUI's `↳` line
        // can render it instead of the raw JSON dump that used to bury
        // the prompt under unreadable RPC payloads.
        assert!(
            note.primary.contains("spawned"),
            "spawn note must lead with `spawned`, got `{}`",
            note.primary,
        );
        let adapter = value["adapter"].as_str().expect("adapter id");
        assert_eq!(ctx.focus.as_deref(), Some(adapter));
        assert!(
            ctx.adapter(adapter).is_some(),
            "spawn must register the adapter"
        );

        // state() should now succeed against the focused adapter.
        let state = dispatch(
            parse("state()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("dispatch state");
        assert!(matches!(state, CmdOutcome::Json(_)));

        // Close it back out — the cleanup logic should drop the tab and
        // clear focus.
        let _ = dispatch(
            parse("session.close()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        );
        assert!(ctx.adapters.is_empty(), "session.close must drop the tab");
        assert!(ctx.focus.is_none(), "session.close must clear focus");
    }
}

#[test]
fn plugins_describe_round_trips_through_dispatcher() {
    // The DSL `plugins.describe("claude-code")` must call `plugin.describe`
    // with the right wire shape and surface the resulting catalog.
    let (client, _server) = in_process_client();
    let mut ctx = ReplCtx::new();
    let outcome = dispatch(
        parse(r#"plugins.describe("claude-code")"#).unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    )
    .expect("dispatch plugins.describe");
    let CmdOutcome::Json(value) = outcome else {
        panic!("expected json outcome, got {outcome:?}");
    };
    assert_eq!(value["plugin"], "claude-code");
    // The claude-code plugin owns its describe() catalog, so the
    // server-side fallback path should not have run — we expect at
    // minimum the canonical `send_prompt` intent and the
    // `wait_turn_matcher` wait function.
    let intent_names: Vec<&str> = value["intents"]
        .as_array()
        .expect("intents array")
        .iter()
        .filter_map(|entry| entry.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        intent_names.contains(&"send_prompt"),
        "describe missing send_prompt: {intent_names:?}"
    );
    let wait_names: Vec<&str> = value["wait_matchers"]
        .as_array()
        .expect("wait_matchers array")
        .iter()
        .filter_map(|entry| entry.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        wait_names.contains(&"wait_turn_matcher"),
        "describe missing wait_turn_matcher: {wait_names:?}"
    );
}

#[test]
#[cfg(unix)]
fn turn_dispatches_send_then_wait_atomically() {
    // End-to-end smoke test: `turn(...)` must route through
    // `adapter.turn`. The claude-code wait_turn_matcher anchors on
    // claude-specific turn-end markers that /bin/sh can't emit, so we
    // assert the RPC reached the server (matcher timed out -> -32001)
    // rather than a successful wait. A timeout error proves the
    // serialization path is intact: dispatcher → adapter.turn → plugin
    // dispatch → matcher loop. The wire-shape contract itself is
    // pinned by the unit tests in src/repl/command.rs.
    let (client, _server) = in_process_client();
    let mut ctx = ReplCtx::new();

    let outcome = dispatch(
        parse(
            r#"session.spawn("claude-code", program="/bin/sh", args=["-lc", "printf ready; cat"])"#,
        )
        .unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    )
    .expect("dispatch session.spawn");
    let CmdOutcome::Note { value, .. } = outcome else {
        panic!("expected note outcome from session.spawn, got {outcome:?}");
    };
    let adapter = value["adapter"].as_str().expect("adapter id").to_string();

    // Tight 250 ms wait so the test fails fast on regression but still
    // reaches the matcher loop.
    let result = dispatch(
        parse(r#"turn("send_prompt", prompt="probe\n", wait=matches(r"never-matches"), timeout=250ms)"#)
            .unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    );
    match result {
        Err(error) => {
            // -32001 is the matcher timeout code defined in src/rpc.rs.
            // Anything else means the dispatcher rejected the call
            // before it reached the server.
            let message = error.to_string();
            assert!(
                message.contains("-32001") || message.contains("matcher"),
                "expected matcher timeout from adapter.turn, got `{message}`"
            );
        }
        Ok(other) => panic!("expected -32001 matcher timeout, got {other:?}"),
    }

    let _ = dispatch(
        parse(&format!(r#"session.close("{adapter}")"#)).unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    );
}

#[test]
fn notifications_meta_forwards_filter_to_server() {
    // `:notifications on adapters=e1,e2 sessions=s1` must serialize to
    // a `server.set_notifications` call carrying the filter arrays.
    // The server echoes the resolved filter back, so we can assert
    // round-trip equality.
    let (client, _server) = in_process_client();
    let mut ctx = ReplCtx::new();
    let outcome = dispatch(
        parse(":notifications on adapters=e1,e2 sessions=s1").unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    )
    .expect("dispatch :notifications");
    let CmdOutcome::Json(value) = outcome else {
        panic!("expected json from :notifications, got {outcome:?}");
    };
    assert_eq!(value["enabled"], true);
    assert_eq!(value["adapters"], json!(["e1", "e2"]));
    assert_eq!(value["sessions"], json!(["s1"]));
}

#[test]
fn rpc_meta_passthrough_invokes_server() {
    let (client, _server) = in_process_client();
    let mut ctx = ReplCtx::new();
    let outcome = dispatch(
        parse(":rpc server.capabilities").unwrap(),
        &client,
        &mut ctx,
        Duration::from_secs(5),
    )
    .expect("dispatch :rpc");
    let CmdOutcome::Json(value) = outcome else {
        panic!("expected json outcome")
    };
    assert_eq!(value["name"], "ptywright");
}

#[test]
#[cfg(unix)]
fn notifications_subscription_fires_for_adapter_sessions() {
    let (client, _server) = in_process_client();
    let notifications = client.notifications();

    let _ = client
        .call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        )
        .expect("enable notifications");

    let start = client
        .call(
            "adapter.start",
            json!({
                "plugin": "claude-code",
                "program": "/bin/sh",
                "args": ["-lc", "printf 'repl-notif-fixture\\n' && cat"],
            }),
            Duration::from_secs(5),
        )
        .expect("adapter.start");
    let adapter = start["adapter"].as_str().expect("adapter id").to_string();
    let session = start["session"].as_str().expect("session id").to_string();

    // Keep poking the server so it has reason to flush a notification
    // batch — adapter.state is harmless and re-classifies under the hood.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_changed = false;
    while std::time::Instant::now() < deadline && !saw_changed {
        let _ = client.call(
            "adapter.state",
            json!({ "adapter": adapter }),
            Duration::from_secs(5),
        );
        if let Ok(notification) = notifications.recv_timeout(Duration::from_millis(100))
            && notification.method == "session.changed"
            && notification.params["session"] == session.as_str()
        {
            saw_changed = true;
        }
    }
    assert!(
        saw_changed,
        "expected session.changed notification for adapter session `{session}` within 5s"
    );

    let _ = client.call(
        "adapter.close",
        json!({ "adapter": adapter }),
        Duration::from_secs(5),
    );
}

#[test]
fn capabilities_advertises_session_output_notification() {
    let (client, _server) = in_process_client();
    let capabilities = client
        .call("server.capabilities", json!({}), Duration::from_secs(5))
        .expect("server.capabilities");
    let notifications = capabilities["notifications"]
        .as_array()
        .expect("notifications array");
    let names: Vec<&str> = notifications.iter().filter_map(|n| n.as_str()).collect();
    assert!(
        names.contains(&"session.output"),
        "capabilities.notifications must include `session.output`; got {names:?}",
    );
}

#[test]
#[cfg(unix)]
fn session_output_notification_carries_emitted_text() {
    let (client, _server) = in_process_client();
    let notifications = client.notifications();

    let _ = client
        .call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        )
        .expect("enable notifications");

    // Spawn a tiny fixture that emits a distinctive token then idles so the
    // PTY stays open long enough for the notification to flush. `cat` keeps
    // the child alive without further output.
    let start = client
        .call(
            "adapter.start",
            json!({
                "plugin": "claude-code",
                "program": "/bin/sh",
                "args": ["-lc", "printf 'output-notif-fixture\\n' && cat"],
            }),
            Duration::from_secs(5),
        )
        .expect("adapter.start");
    let adapter = start["adapter"].as_str().expect("adapter id").to_string();
    let session = start["session"].as_str().expect("session id").to_string();

    // Poke the server periodically so notification polling flushes between
    // requests — mirrors the existing `session.changed` integration test.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut collected = String::new();
    let mut last_sequence: u64 = 0;
    while std::time::Instant::now() < deadline && !collected.contains("output-notif-fixture") {
        let _ = client.call(
            "adapter.state",
            json!({ "adapter": adapter }),
            Duration::from_secs(5),
        );
        if let Ok(notification) = notifications.recv_timeout(Duration::from_millis(100))
            && notification.method == "session.output"
            && notification.params["session"] == session.as_str()
        {
            let seq = notification.params["sequence"]
                .as_u64()
                .expect("sequence field");
            assert!(
                seq >= last_sequence,
                "session.output sequences must be monotonic; got {seq} after {last_sequence}",
            );
            last_sequence = seq;
            if let Some(text) = notification.params["output"].as_str() {
                collected.push_str(text);
            }
        }
    }
    assert!(
        collected.contains("output-notif-fixture"),
        "expected session.output to carry fixture text within 5s; got: {collected:?}",
    );

    let _ = client.call(
        "adapter.close",
        json!({ "adapter": adapter }),
        Duration::from_secs(5),
    );
}

#[test]
#[cfg(unix)]
fn session_output_redacts_secret_shaped_tokens_by_default() {
    // The notification has no caller-supplied redaction parameter, so the
    // server applies the default `RedactionPolicy` — mirroring the default
    // for `adapter.transcript`. A bare-eye `token=` value in PTY output
    // must reach the wire as `[REDACTED]`, not the raw secret. This guards
    // the user-visible "default-redacted" contract the PR documents.
    let (client, _server) = in_process_client();
    let notifications = client.notifications();
    let _ = client
        .call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        )
        .expect("enable notifications");

    let start = client
        .call(
            "adapter.start",
            json!({
                "plugin": "claude-code",
                "program": "/bin/sh",
                "args": ["-lc", "printf 'token=super-secret\\n' && cat"],
            }),
            Duration::from_secs(5),
        )
        .expect("adapter.start");
    let adapter = start["adapter"].as_str().expect("adapter id").to_string();
    let session = start["session"].as_str().expect("session id").to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut collected = String::new();
    while std::time::Instant::now() < deadline {
        let _ = client.call(
            "adapter.state",
            json!({ "adapter": adapter }),
            Duration::from_secs(5),
        );
        if let Ok(notification) = notifications.recv_timeout(Duration::from_millis(100))
            && notification.method == "session.output"
            && notification.params["session"] == session.as_str()
            && let Some(text) = notification.params["output"].as_str()
        {
            collected.push_str(text);
            if collected.contains("[REDACTED]") {
                break;
            }
        }
    }

    assert!(
        collected.contains("[REDACTED]"),
        "session.output must redact secret-shaped tokens by default; got {collected:?}"
    );
    assert!(
        !collected.contains("super-secret"),
        "raw secret leaked through default redaction; got {collected:?}"
    );

    let _ = client.call(
        "adapter.close",
        json!({ "adapter": adapter }),
        Duration::from_secs(5),
    );
}

#[test]
#[cfg(unix)]
fn session_output_first_emission_skips_pre_subscription_buffer() {
    // Output produced *before* `server.set_notifications { enabled: true }`
    // must not arrive in the first delivery — otherwise a long-lived
    // session would dump its entire 128 KiB retained transcript on
    // subscription. We seed the per-connection cursor at the current
    // `chars_written` when notifications flip on, so the very first
    // payload only carries what landed afterwards.
    let (client, _server) = in_process_client();
    let notifications = client.notifications();

    // Start the adapter *before* enabling notifications and let it
    // produce a known marker. That marker must not appear in subsequent
    // `session.output` frames.
    let start = client
        .call(
            "adapter.start",
            json!({
                "plugin": "claude-code",
                "program": "/bin/sh",
                "args": [
                    "-lc",
                    "printf 'pre-subscription-marker\\n'; sleep 0.3; printf 'post-subscription-marker\\n'; cat",
                ],
            }),
            Duration::from_secs(5),
        )
        .expect("adapter.start");
    let adapter = start["adapter"].as_str().expect("adapter id").to_string();
    let session = start["session"].as_str().expect("session id").to_string();

    // Wait long enough for the pre-marker to land in the transcript.
    let _ = client.call(
        "adapter.state",
        json!({ "adapter": adapter }),
        Duration::from_secs(5),
    );
    std::thread::sleep(Duration::from_millis(150));
    let transcript = client
        .call(
            "adapter.transcript",
            json!({ "adapter": adapter, "redact": false }),
            Duration::from_secs(5),
        )
        .expect("adapter.transcript");
    let pre_in_transcript = transcript["text"]
        .as_str()
        .unwrap_or("")
        .contains("pre-subscription-marker");
    assert!(
        pre_in_transcript,
        "fixture sequencing assumption: pre-marker must be in the transcript before we subscribe"
    );

    // Subscribe — this should snap the cursor to current `chars_written`.
    let _ = client
        .call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        )
        .expect("enable notifications");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut collected = String::new();
    while std::time::Instant::now() < deadline && !collected.contains("post-subscription-marker") {
        let _ = client.call(
            "adapter.state",
            json!({ "adapter": adapter }),
            Duration::from_secs(5),
        );
        if let Ok(notification) = notifications.recv_timeout(Duration::from_millis(100))
            && notification.method == "session.output"
            && notification.params["session"] == session.as_str()
            && let Some(text) = notification.params["output"].as_str()
        {
            collected.push_str(text);
        }
    }

    assert!(
        collected.contains("post-subscription-marker"),
        "expected post-subscription output to arrive via session.output; got {collected:?}"
    );
    assert!(
        !collected.contains("pre-subscription-marker"),
        "session.output must not replay pre-subscription buffer; got {collected:?}"
    );

    let _ = client.call(
        "adapter.close",
        json!({ "adapter": adapter }),
        Duration::from_secs(5),
    );
}

#[test]
#[cfg(unix)]
fn session_output_flags_dropped_when_buffer_evicts_unseen_range() {
    // Tiny transcript_max_chars guarantees eviction of the unseen range
    // between subscription and the first delivery, so the next
    // `session.output` carries `dropped: true` and only the surviving
    // tail. This exercises the wire-shape branch end-to-end (the unit
    // test in `transcript::tests` already covers the data-structure side).
    let (client, _server) = in_process_client();
    let notifications = client.notifications();

    // Use session.create so we can dial the retention down to 32 chars —
    // `adapter.start` doesn't take a transcript_max_chars option today
    // and the default 128 KiB is far too generous for this scenario.
    // Produce ~1 KiB of output spread across short bursts so the ring
    // buffer evicts before we ever poll the first notification.
    let _ = client
        .call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        )
        .expect("enable notifications");

    let create = client
        .call(
            "session.create",
            json!({
                "program": "/bin/sh",
                "args": [
                    "-lc",
                    "for i in $(seq 1 20); do printf 'output-burst-%02d-padding-padding-padding-padding-padding\\n' \"$i\"; done; cat",
                ],
                "transcript_max_chars": 32,
            }),
            Duration::from_secs(5),
        )
        .expect("session.create");
    let session = create["session"].as_str().expect("session id").to_string();

    // Wait until we actually see a `dropped: true` notification.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_dropped = false;
    while std::time::Instant::now() < deadline && !saw_dropped {
        let _ = client.call("server.capabilities", json!({}), Duration::from_secs(5));
        if let Ok(notification) = notifications.recv_timeout(Duration::from_millis(100))
            && notification.method == "session.output"
            && notification.params["session"] == session.as_str()
            && notification.params["dropped"].as_bool() == Some(true)
        {
            saw_dropped = true;
        }
    }

    assert!(
        saw_dropped,
        "expected at least one session.output with dropped=true given a 32-char retention buffer fed ~1 KiB of output"
    );

    let _ = client.call(
        "session.close",
        json!({ "session": session }),
        Duration::from_secs(5),
    );
}
