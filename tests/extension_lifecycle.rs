//! Lifecycle tests for [`ExtensionHandle`] — focus on what happens
//! to in-flight waits when the underlying [`Session`] terminates or
//! a [`CancellationToken`] flips from another thread.
//!
//! The motivating concerns:
//!
//! 1. A consumer (Claudette, claude-stream, third-party) calls
//!    `adapter.wait` with a long timeout. The user kills the child
//!    process (or the child exits on its own). The wait must unblock
//!    promptly rather than sit on the original timeout window.
//!
//! 2. The same consumer issues `adapter.cancel_wait { wait_id }`
//!    from another connection. The pending wait must return
//!    `Err(Cancelled)` within one polling interval, even with no
//!    PTY bytes arriving.
//!
//! 3. After the handle's session exits, subsequent calls (`state`,
//!    `wait`) must surface a meaningful error rather than deadlocking
//!    or returning stale snapshots.
//!
//! These properties are already covered for raw [`Session`] inside
//! `src/session.rs::tests`; this file pins them at the
//! `ExtensionHandle` layer to make sure plugin classifier hooks don't
//! introduce additional lock contention or hang paths between the
//! matcher loop and the host's terminal classify call.
//!
//! `ExtensionHandle` is `Send` but not `Sync` — plugins
//! (`LuaExtension`) wrap `mlua::Lua` which is not safe to share
//! across threads without external synchronisation. The
//! cancellation token, on the other hand, is `Clone + Send + Sync`,
//! which is the lever we use to test cross-thread signalling: the
//! handle moves into the worker that drives the wait, and the
//! main thread interacts only via the token.
//!
//! Plugin choice: uses the built-in `claude-code` plugin because it's
//! the only `LuaExtension` we ship. The fixture child is a plain
//! shell sleep — it never emits any of the screens the classifier
//! looks for, so `wait_turn_matcher` will never fire on its own.
//! That's intentional: we're exercising the unblock paths, not the
//! match path.

#![cfg(unix)]

use std::thread;
use std::time::{Duration, Instant};

use ptywright::{
    CancellationToken, Error, ExtensionHandle, LuaExtension, Session, SessionConfig, Target,
};

const STABLE_MS: u64 = 300;

fn sleep_target(seconds: u32) -> Target {
    Target::new("/bin/sh").args(["-lc", &format!("sleep {seconds}")])
}

fn build_handle(sleep_seconds: u32) -> ExtensionHandle {
    let session =
        Session::spawn(SessionConfig::new(sleep_target(sleep_seconds))).expect("spawn sleep");
    let extension = LuaExtension::built_in("claude-code").expect("load claude-code");
    ExtensionHandle::start(Box::new(extension), session, STABLE_MS)
}

#[test]
fn wait_settles_promptly_when_underlying_session_exits() {
    // Spawn a short-lived shell. `wait_cancel_settled_matcher`
    // returns a `screen_stable(completed_turn_stable_ms)` matcher,
    // which is satisfied once the rendered screen has been quiet for
    // the configured window. After the child exits no more bytes
    // arrive, so the screen IS stable and the wait should succeed
    // shortly after — proving that session exit unblocks
    // ExtensionHandle's wait loop without sitting on the timeout.
    //
    // We do NOT pin Error::Closed here: matchers with a non-None
    // `minimum_stable_duration` deliberately keep polling past
    // process exit so the stability window can complete (so the
    // caller sees a real match rather than a synthetic "closed").
    // What matters for lifecycle correctness is bounded latency —
    // a matcher tied only to stability must converge within
    // child-exit + stable_window + epsilon, not the full timeout.
    let handle = build_handle(1);

    let started = Instant::now();
    let result = handle.wait(
        "wait_cancel_settled_matcher",
        serde_json::json!({}),
        Duration::from_secs(30),
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "child exit + stability window must converge promptly; elapsed={elapsed:?}, result={result:?}"
    );
    match result {
        Ok((_, _)) => {}
        Err(Error::Closed) => {}
        other => panic!(
            "expected the stability matcher to fire (or surface Closed); got {other:?} after {elapsed:?}"
        ),
    }
}

#[test]
fn wait_with_cancel_returns_cancelled_from_sibling_thread() {
    // Long-running session + a cancel token flipped from a sibling
    // thread. The wait must return Err(Cancelled) within one polling
    // interval — proving that ExtensionHandle's classifier hook
    // forwards the token down into Session::wait_for_inner without
    // any added latency from the plugin path.
    let handle = build_handle(60);
    let token = CancellationToken::new();
    let canceller = token.clone();

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(120));
        canceller.cancel();
    });

    let started = Instant::now();
    let result = handle.wait_with_cancel(
        "wait_turn_matcher",
        serde_json::json!({}),
        Duration::from_secs(30),
        &token,
    );
    let elapsed = started.elapsed();

    match result {
        Err(Error::Cancelled) => {}
        other => panic!("expected Error::Cancelled, got {other:?} after {elapsed:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancellation must wake the wait within the poll interval; elapsed={elapsed:?}"
    );

    let _ = handle.session().terminate(Duration::from_secs(2));
}

#[test]
fn state_remains_callable_after_session_exits() {
    // A consumer that polls `adapter.state` after the underlying
    // session has exited must still receive a usable snapshot — the
    // classifier sees an empty / terminated screen but should not
    // panic or deadlock. This is what protects callers like Claudette
    // from a `cargo test` invocation that exits the agent mid-poll.
    let handle = build_handle(1);

    // Give the child time to exit on its own.
    thread::sleep(Duration::from_millis(1500));

    // `state()` should not block forever and should surface a
    // snapshot (possibly a `starting` / `plugin_error` fallback).
    let started = Instant::now();
    let snap = handle.state();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "state() must not block after the session has exited"
    );
    assert!(
        !snap.state.is_empty(),
        "state classification should still be non-empty after exit"
    );
}

#[test]
fn dropping_handle_after_wait_succeeds_cleans_up_session() {
    // Smoke test: an entire ExtensionHandle lifecycle — build,
    // initiate-then-cancel a wait, drop. Verifies no panic on Drop
    // for the LuaPlugin registry, the wait token, or the underlying
    // Session, and that the PTY child terminates within bounded
    // time.
    let pid = {
        let handle = build_handle(60);
        let session_pid = handle
            .session()
            .pid()
            .expect("expected a pid for shell child");
        let token = CancellationToken::new();
        let canceller = token.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            canceller.cancel();
        });
        let _ = handle.wait_with_cancel(
            "wait_turn_matcher",
            serde_json::json!({}),
            Duration::from_secs(10),
            &token,
        );
        let _ = handle.session().terminate(Duration::from_secs(2));
        session_pid
    };

    // Best-effort assertion: the PTY child PID should no longer be
    // alive a beat after the handle drops. Use kill(pid, 0) — a
    // POSIX existence probe that doesn't actually send a signal.
    thread::sleep(Duration::from_millis(300));
    let alive = unsafe {
        // SAFETY: kill(pid, 0) is a stat-only operation. We treat any
        // negative return (errno=ESRCH) as "process gone".
        libc::kill(pid as libc::pid_t, 0) == 0
    };
    assert!(
        !alive,
        "shell child (pid {pid}) should be reaped after handle drop"
    );
}
