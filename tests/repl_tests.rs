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
        let CmdOutcome::Json(value) = outcome else {
            panic!("expected json outcome")
        };
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
