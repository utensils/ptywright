//! ratatui-based REPL: entry point and main event loop.
//!
//! The REPL is a full-screen TUI:
//!
//! ```text
//! ┌─ Tab strip ─────────────────────────────────────┐
//! ├─ Live snapshot pane (focused adapter) ──────────┤
//! ├─ Command log (pty> + ↳ notes) ──────────────────┤
//! ├─ Input box (DSL prompt) ────────────────────────┤
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! ratatui owns the whole screen and redraws one full frame per loop
//! iteration; crossterm events drive both the input widget and the
//! command dispatcher. A background thread converts server notifications
//! into [`UiEvent`]s and fetches snapshots when the focused adapter
//! changes — those events drain into the main loop between draws.
//!
//! No claude-code (or other application-specific) identifiers live in
//! this file. Adapter ids, plugin names, intent strings, and state labels
//! all flow through as opaque strings against the generic `adapter.*`
//! RPC surface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use serde_json::json;

use super::app::App;
use super::completer::{AdapterCache, PluginCache, ReplCompleter};
use super::ctx::ReplCtx;
use super::dispatcher::{UiEvent, spawn_dispatcher};
use super::history::ReplHistory;
use super::transport::RpcClient;
use crate::error::{Error, Result};
use crate::paths::Paths;

/// Server-side calls block on the operator's network — 30 s is enough
/// headroom for the initial probes (`adapter.list`, `adapter.live`) and
/// for commands like `wait(matches(...))` that may legitimately stall.
pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the REPL pings the server to keep its notification pump warm.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// How long to block on a terminal event before re-drawing. Short enough
/// that pending [`UiEvent`]s feel instant; long enough that an idle REPL
/// does not spin a CPU core.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Public entry. Builds the TUI, runs the event loop, and restores the
/// terminal on return.
pub fn run(client: Arc<RpcClient>, transport_label: String) -> Result<()> {
    let ctx = Arc::new(Mutex::new(ReplCtx::new()));
    let plugins = PluginCache::new();
    let adapters_cache = AdapterCache::new();

    // Seed the plugin cache so the very first Tab inside
    // `session.spawn("…")` knows which plugin names exist.
    if let Ok(value) = client.call("adapter.list", json!({}), RPC_TIMEOUT)
        && let Some(names) = extract_plugin_names(&value)
    {
        plugins.set(names);
    }

    // Subscribe to server notifications and probe for live adapters.
    let _ = client.call(
        "server.set_notifications",
        json!({ "enabled": true }),
        RPC_TIMEOUT,
    );
    client.start_heartbeat(HEARTBEAT_INTERVAL);
    let live_hint = match client.call("adapter.live", json!({}), RPC_TIMEOUT) {
        Ok(value) => {
            adapters_cache.set(extract_live_ids(&value));
            format_live_hint(&value)
        }
        Err(_) => None,
    };

    // Persistent history at `~/.ptywright/repl-history`.
    let history_path = Paths::from_env().repl_history_path();
    let history = ReplHistory::open(&history_path)?;
    let completer = ReplCompleter::new(Arc::clone(&ctx), plugins.clone(), adapters_cache.clone());

    // Notification + snapshot dispatcher. Pushes UiEvents into the main
    // loop; main loop pushes "focus changed" hints back so the dispatcher
    // knows which adapter to snapshot on `session.changed`.
    let (ui_tx, ui_rx) = crossbeam_channel::unbounded::<UiEvent>();
    let (focus_tx, focus_rx) = crossbeam_channel::unbounded::<Option<String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let _dispatcher = spawn_dispatcher(
        Arc::clone(&client),
        client.notifications(),
        ui_tx.clone(),
        focus_rx,
        Arc::clone(&stop),
    );
    let _stop_guard = StopGuard(Arc::clone(&stop));

    let mut app = App::new(
        Arc::clone(&client),
        Arc::clone(&ctx),
        completer,
        history,
        transport_label,
        ui_tx.clone(),
        focus_tx,
        plugins,
        adapters_cache,
    );
    app.set_live_hint(live_hint);

    let mut terminal = ratatui::try_init().map_err(io_to_err)?;
    let result = event_loop(&mut terminal, &mut app, &ui_rx);
    let _ = ratatui::try_restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    ui_rx: &crossbeam_channel::Receiver<UiEvent>,
) -> Result<()> {
    while !app.should_quit() {
        terminal
            .draw(|frame| super::render::render(frame, app))
            .map_err(io_to_err)?;

        // Drain background events without blocking — these update tabs,
        // snapshots, and the log between frames.
        while let Ok(event) = ui_rx.try_recv() {
            app.apply_ui_event(event);
        }

        if event::poll(POLL_INTERVAL).map_err(io_to_err)? {
            match event::read().map_err(io_to_err)? {
                Event::Key(key) => app.handle_key(key),
                Event::Resize(_, _) => { /* ratatui redraws on next iter */ }
                Event::Paste(text) => app.handle_paste(&text),
                _ => {}
            }
        }
    }
    Ok(())
}

fn io_to_err(err: std::io::Error) -> Error {
    Error::Rpc(format!("tui io: {err}"))
}

/// RAII helper that signals the dispatcher thread to exit whenever the
/// main loop returns, including on panic. The dispatcher reads the flag
/// between notification polls and tears itself down cleanly.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// ---- startup probes ----------------------------------------------------

fn extract_plugin_names(value: &serde_json::Value) -> Option<Vec<String>> {
    let plugins = value.get("plugins")?.as_array()?;
    Some(
        plugins
            .iter()
            .filter_map(|p| {
                p.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect(),
    )
}

fn extract_live_ids(value: &serde_json::Value) -> Vec<String> {
    let Some(adapters) = value.get("adapters").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    adapters
        .iter()
        .filter(|entry| {
            !entry
                .get("finished")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            entry
                .get("adapter")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty())
        .collect()
}

fn format_live_hint(value: &serde_json::Value) -> Option<String> {
    let ids = extract_live_ids(value);
    if ids.is_empty() {
        return None;
    }
    let joined = ids.join(", ");
    Some(format!(
        "{} live adapter(s) on the server: {} — `:attach all` to load them, or `:attach <id>`",
        ids.len(),
        joined,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn live_hint_is_none_when_no_adapters() {
        assert!(format_live_hint(&json!({ "adapters": [] })).is_none());
        assert!(format_live_hint(&json!({})).is_none());
    }

    #[test]
    fn live_hint_lists_running_adapter_ids() {
        let value = json!({
            "adapters": [
                { "adapter": "e1", "plugin": "claude-code", "finished": false, "session": "s1", "sequence": 7 },
                { "adapter": "e2", "plugin": "claude-code", "finished": false, "session": "s2", "sequence": 2 }
            ]
        });
        let hint = format_live_hint(&value).expect("hint present");
        assert!(hint.contains("e1"));
        assert!(hint.contains("e2"));
        assert!(hint.contains(":attach"));
    }

    #[test]
    fn live_ids_drops_malformed_or_finished_entries() {
        let value = json!({
            "adapters": [
                { "adapter": "e1", "finished": false },
                { "adapter": "e2", "finished": true },
                { "adapter": "", "finished": false },
                { "plugin": "claude-code", "finished": false },
                "not-an-object",
                { "adapter": "e3", "finished": false }
            ]
        });
        let ids = extract_live_ids(&value);
        assert_eq!(ids, vec!["e1".to_string(), "e3".to_string()]);
    }
}
