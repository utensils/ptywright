//! Background "screen pump" that keeps the live preview pane current.
//!
//! On startup the caller enables `server.set_notifications`, then spawns
//! this thread with handles to:
//!
//! - the [`super::transport::RpcClient`] (to request `adapter.snapshot`),
//! - the shared [`super::ctx::ReplCtx`] (to read the focused adapter id),
//! - a [`SnapshotStore`] (to write the resulting screen),
//! - a small redraw channel that the TUI event loop selects on.
//!
//! The pump reacts to every `session.changed` / `session.exited`
//! notification *and* polls every 500 ms as a backstop in case
//! notifications are silent. Both paths take the same one-shot snapshot
//! action so a stuck subscription cannot freeze the preview.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError};
use serde_json::{Value, json};

use super::ctx::ReplCtx;
use super::transport::RpcClient;
use crate::screen::ScreenSnapshot;

const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Owned screen-snapshot store. The pump thread writes into it, the TUI
/// renderer reads from it. Two locks — one for the map of all adapters
/// and one for the focused-redraw signal — keep contention minimal.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    inner: RwLock<HashMap<String, ScreenSnapshot>>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap clone of one adapter's latest snapshot, if known.
    pub fn get(&self, adapter: &str) -> Option<ScreenSnapshot> {
        self.inner
            .read()
            .ok()
            .and_then(|guard| guard.get(adapter).cloned())
    }

    pub fn set(&self, adapter: &str, snapshot: ScreenSnapshot) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert(adapter.to_string(), snapshot);
        }
    }

    pub fn forget(&self, adapter: &str) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(adapter);
        }
    }
}

/// Spawn the pump thread. The returned `JoinHandle` should be dropped only
/// when the caller drops the `RpcClient` — the pump exits naturally once
/// the notification receiver disconnects (i.e. the transport closes).
pub fn spawn(
    client: Arc<RpcClient>,
    ctx: Arc<Mutex<ReplCtx>>,
    store: Arc<SnapshotStore>,
    redraw: Sender<()>,
) -> JoinHandle<()> {
    let notifications = client.notifications();
    std::thread::Builder::new()
        .name("ptywright-repl-snapshot-pump".into())
        .spawn(move || {
            loop {
                match notifications.recv_timeout(IDLE_POLL_INTERVAL) {
                    Ok(notification) => {
                        if matches!(
                            notification.method.as_str(),
                            "session.changed" | "session.exited"
                        ) {
                            refresh_focused(&client, &ctx, &store, &redraw);
                            // session.exited cleanup: forget the cached
                            // snapshot so the preview pane doesn't display
                            // a stale screen for a terminated adapter.
                            if notification.method == "session.exited" {
                                purge_exited(&notification.params, &ctx, &store);
                                ping(&redraw);
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Idle tick — backstop in case notifications never
                        // arrive. Still cheap: at most one `adapter.snapshot`
                        // per 500 ms.
                        refresh_focused(&client, &ctx, &store, &redraw);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // Transport closed — nothing to do but exit.
                        break;
                    }
                }
            }
        })
        .expect("spawn snapshot pump thread")
}

fn refresh_focused(
    client: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    store: &Arc<SnapshotStore>,
    redraw: &Sender<()>,
) {
    let focused = ctx.lock().ok().and_then(|guard| guard.focus.clone());
    let Some(adapter) = focused else {
        return;
    };
    match client.call(
        "adapter.snapshot",
        json!({ "adapter": adapter, "redact": false }),
        SNAPSHOT_TIMEOUT,
    ) {
        Ok(value) => match serde_json::from_value::<ScreenSnapshot>(value) {
            Ok(snapshot) => {
                store.set(&adapter, snapshot);
                ping(redraw);
            }
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    adapter,
                    "snapshot pump: dropping malformed adapter.snapshot response",
                );
            }
        },
        Err(error) => {
            tracing::debug!(
                error = %error,
                adapter,
                "snapshot pump: adapter.snapshot call failed",
            );
        }
    }
}

fn purge_exited(params: &Value, ctx: &Arc<Mutex<ReplCtx>>, store: &Arc<SnapshotStore>) {
    let Some(session) = params.get("session").and_then(Value::as_str) else {
        return;
    };
    if let Ok(guard) = ctx.lock() {
        // We don't store session id on AdapterTab today (the dispatcher
        // tracks it only on the response). Best-effort: forget every
        // adapter snapshot to avoid stale rendering, the next refresh
        // will repopulate the active one.
        let _ = session;
        for tab in &guard.adapters {
            store.forget(&tab.id);
        }
    }
}

fn ping(sender: &Sender<()>) {
    // sync_channel(1)-style semantics: a queued tick is enough; coalesce
    // additional pings into a single redraw.
    match sender.try_send(()) {
        Ok(_) | Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::Framing;
    use crate::repl::transport::RpcClient;
    use crate::rpc::{RpcServerState, serve_ndjson_with_state};
    use crossbeam_channel::bounded;
    use std::io::pipe;
    use std::time::Instant;

    fn in_process_client() -> (Arc<RpcClient>, std::thread::JoinHandle<()>) {
        let (c2s_r, c2s_w) = pipe().expect("c2s pipe");
        let (s2c_r, s2c_w) = pipe().expect("s2c pipe");
        let state = RpcServerState::new();
        let server = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
        });
        let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
        (client, server)
    }

    #[test]
    fn store_round_trips_set_get_forget() {
        let store = SnapshotStore::new();
        assert!(store.get("e1").is_none());
        let dummy = ScreenSnapshot {
            size: crate::target::TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 0,
                pixel_height: 0,
            },
            cursor: crate::screen::CursorState {
                row: 0,
                col: 0,
                visible: false,
            },
            sequence: 7,
            plain_text: "x".into(),
            cells: Vec::new(),
            alternate_screen: false,
            application_cursor: false,
            application_keypad: false,
            title: None,
        };
        store.set("e1", dummy.clone());
        assert_eq!(store.get("e1").map(|s| s.sequence), Some(7));
        store.forget("e1");
        assert!(store.get("e1").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn pump_refreshes_focused_adapter_snapshot() {
        let (client, _server) = in_process_client();
        let ctx = Arc::new(Mutex::new(ReplCtx::new()));
        let store = Arc::new(SnapshotStore::new());
        let (redraw_tx, redraw_rx) = bounded::<()>(1);

        // Spawn an adapter against a `cat` fixture and focus it. The pump
        // should publish a snapshot within a couple of idle ticks.
        let start = client
            .call(
                "adapter.start",
                json!({
                    "plugin": "claude-code",
                    "program": "/bin/sh",
                    "args": ["-lc", "printf 'pump-fixture\\n' && cat"],
                }),
                Duration::from_secs(5),
            )
            .expect("adapter.start");
        let adapter = start["adapter"].as_str().expect("adapter id").to_string();
        {
            let mut guard = ctx.lock().unwrap();
            guard.upsert_adapter(&adapter, "claude-code");
            guard.focus = Some(adapter.clone());
        }

        let _ = client.call(
            "server.set_notifications",
            json!({ "enabled": true }),
            Duration::from_secs(5),
        );

        let _pump = spawn(
            Arc::clone(&client),
            Arc::clone(&ctx),
            Arc::clone(&store),
            redraw_tx,
        );

        // Wait for either a redraw ping or a snapshot to appear in the
        // store, whichever comes first.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if store.get(&adapter).is_some() {
                break;
            }
            if Instant::now() >= deadline {
                panic!("pump never published a snapshot within 5s");
            }
            let _ = redraw_rx.recv_timeout(Duration::from_millis(100));
        }

        // Close the adapter so the fixture shell exits cleanly.
        let _ = client.call(
            "adapter.close",
            json!({ "adapter": adapter }),
            Duration::from_secs(5),
        );
    }
}
