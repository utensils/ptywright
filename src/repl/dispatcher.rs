//! Background thread that bridges server notifications + snapshot fetches
//! to the ratatui main loop.
//!
//! One thread, two responsibilities:
//!
//! 1. **Notification filter** — consumes `RpcClient::notifications()`,
//!    drops `session.output` and `session.changed` (handled by the live
//!    pane), keeps `session.exited` and unknown plugin notifications,
//!    forwards survivors as [`UiEvent::Notice`].
//! 2. **Snapshot fetcher** — on `session.changed` for the currently
//!    focused adapter, calls `adapter.snapshot` and forwards the result
//!    as [`UiEvent::Snapshot`] for the main loop to drop into its cache.
//!
//! Focus is tracked via a separate channel from the main loop: whenever
//! the operator's command shifts focus (`session.spawn`, `:focus`, etc.),
//! the main loop sends the new focused adapter id; the dispatcher reads
//! the latest value before issuing the snapshot RPC.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::{Value, json};

use super::transport::{Notification, RpcClient};
use crate::screen::ScreenSnapshot;

/// Event surfaced to the main loop. Plain enum so the main thread can
/// match on it without touching mutexes.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Background-fetched snapshot for an adapter. The main loop drops it
    /// into its cache and the next draw renders it in the live pane.
    Snapshot {
        adapter: String,
        snapshot: ScreenSnapshot,
    },
    /// One-line styled notice to append to the log (e.g.
    /// `session.exited`).
    Notice(String),
}

/// How long the dispatcher blocks on its notification recv before
/// re-checking the stop flag. Short enough that exit is responsive,
/// long enough that an idle dispatcher does not spin.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// How long to wait for the server's `adapter.snapshot` response. The
/// call should usually complete in milliseconds; longer waits indicate a
/// stalled session and we want to surface the timeout as a noticeable
/// pause rather than block the dispatcher forever.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn the dispatcher thread. Returns its `JoinHandle` so callers can
/// observe panics; the thread reads `stop` between every recv and exits
/// cleanly when the flag is set.
pub fn spawn_dispatcher(
    client: Arc<RpcClient>,
    notifications: Receiver<Notification>,
    ui_tx: Sender<UiEvent>,
    focus_rx: Receiver<Option<String>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("ptywright-repl-dispatcher".into())
        .spawn(move || {
            let mut focus: Option<String> = None;
            while !stop.load(Ordering::Relaxed) {
                // Drain pending focus updates before each notification
                // poll so we always see the latest focus when deciding
                // whether to snapshot.
                while let Ok(new_focus) = focus_rx.try_recv() {
                    focus = new_focus;
                    if let Some(ref id) = focus {
                        // Kick an immediate snapshot for the new focus
                        // so the pane fills in without waiting for the
                        // next session.changed.
                        if let Some(snap) = fetch_snapshot(&client, id) {
                            let _ = ui_tx.send(UiEvent::Snapshot {
                                adapter: id.clone(),
                                snapshot: snap,
                            });
                        }
                    }
                }
                match notifications.recv_timeout(RECV_TIMEOUT) {
                    Ok(notification) => {
                        handle_notification(&client, &focus, &ui_tx, &notification);
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .expect("spawn dispatcher thread")
}

fn handle_notification(
    client: &RpcClient,
    focus: &Option<String>,
    ui_tx: &Sender<UiEvent>,
    notification: &Notification,
) {
    match notification.method.as_str() {
        // High-rate, low-signal — absorbed by the live pane.
        "session.output" => {}
        "session.changed" => {
            let adapter = notification
                .params
                .get("adapter")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(adapter) = adapter else {
                return;
            };
            // Only fetch when the change is for the focused adapter.
            // The dispatcher's snapshot cache is per-focus to keep the
            // RPC traffic bounded; if the user switches focus, the
            // pending focus update upstream will trigger a fresh fetch.
            if focus.as_deref() != Some(&adapter) {
                return;
            }
            if let Some(snap) = fetch_snapshot(client, &adapter) {
                let _ = ui_tx.send(UiEvent::Snapshot {
                    adapter,
                    snapshot: snap,
                });
            }
        }
        _ => {
            let line = render_notice(notification);
            let _ = ui_tx.send(UiEvent::Notice(line));
        }
    }
}

fn fetch_snapshot(client: &RpcClient, adapter: &str) -> Option<ScreenSnapshot> {
    let response = client
        .call(
            "adapter.snapshot",
            json!({ "adapter": adapter, "redact": true }),
            SNAPSHOT_TIMEOUT,
        )
        .ok()?;
    let snap = response.get("snapshot").cloned()?;
    serde_json::from_value(snap).ok()
}

/// Render a survivor notification as a one-liner. Plain text (no ANSI
/// here — the main loop applies styling when it paints the log entry).
pub(super) fn render_notice(notification: &Notification) -> String {
    let session = notification
        .params
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let sequence = notification.params.get("sequence").and_then(Value::as_u64);
    match (notification.method.as_str(), sequence) {
        ("session.exited", Some(seq)) => format!("session.exited  {session} seq={seq}"),
        ("session.exited", None) => format!("session.exited  {session}"),
        (other, _) => format!("{other} {}", notification.params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_notice_formats_session_exited() {
        let n = Notification {
            method: "session.exited".into(),
            params: json!({ "session": "s1", "sequence": 9 }),
        };
        assert_eq!(render_notice(&n), "session.exited  s1 seq=9");
    }

    #[test]
    fn render_notice_falls_back_to_method_and_params() {
        let n = Notification {
            method: "future.event".into(),
            params: json!({ "anything": "goes" }),
        };
        let rendered = render_notice(&n);
        assert!(rendered.starts_with("future.event"));
        assert!(rendered.contains("anything"));
    }
}
