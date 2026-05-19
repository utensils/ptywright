//! Sequential reedline-based REPL.
//!
//! Each command is rendered as `pty> <syntax-highlighted DSL>` and the
//! result follows on the next line as `↳ <dim summary>`, matching the
//! original mockup. Line editing, completion, syntax highlighting,
//! history, and ghost-text hinting are all delegated to reedline; this
//! module only handles the read-eval-print loop, the prompt, and how
//! each `CmdOutcome` is printed.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, ExternalPrinter, KeyCode, KeyModifiers, MenuBuilder,
    Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal,
    default_emacs_keybindings,
};
use serde_json::{Value, json};

use super::command::{CmdOutcome, parse};
use super::completer::{AdapterCache, PluginCache, ReplCompleter};
use super::ctx::ReplCtx;
use super::highlighter::ReplHighlighter;
use super::live::{
    AltScreenGuard, LiveLayout, PaintLock, RefreshTx, handle_change_notification, refresh_channel,
    spawn_redraw_thread,
};
use super::transport::{Notification, RpcClient};
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::screen::{ScreenCellStyle, ScreenSnapshot};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the REPL pings the server to keep its notification pump warm.
///
/// The server only polls notifications on inbound requests, so a parked
/// prompt needs a heartbeat for `session.changed` events from other
/// connections to reach us. The chosen value trades responsiveness against
/// per-connection overhead: every tick takes the server's shared-state
/// `Mutex` and walks every session + adapter row. 500 ms is responsive
/// enough for a human at a prompt and keeps a single-user fan-out
/// well under any contention threshold. Bump it if you wire many
/// always-on REPLs against the same server.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Public entry. Builds reedline + prompt and runs the read-eval-print
/// loop until the user quits with `:quit` / `Ctrl-D`.
///
/// Three concurrent surfaces run inside `run`:
///
/// 1. **Reedline** owns the bottom region — prompt, input, log
///    scrolling — and reads the keyboard.
/// 2. **Notification printer thread** consumes the broadcast channel
///    from [`RpcClient`], filters notification noise (see
///    [`format_notification`]), and forwards the few survivors to
///    reedline's external printer.
/// 3. **Live-pane redraw thread** (`src/repl/live.rs`) paints the top
///    region — tab strip, focused snapshot, divider — on every
///    `session.changed` notification (debounced) and on every command
///    boundary.
///
/// The alt-screen + DECSTBM scroll region keep these three writers
/// from clobbering each other: reedline's `MoveTo` calls land inside
/// the scroll region, the redraw thread's `MoveTo` calls land in the
/// fixed top region, and both bracket their paints with DEC save /
/// restore cursor so the prompt origin survives concurrent updates.
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

    // Subscribe to server notifications by default — most users want to
    // see `session.changed` / `session.exited` events for the adapters
    // they're watching. The companion heartbeat below keeps the pump
    // warm so events from sibling connections actually reach us.
    let _ = client.call(
        "server.set_notifications",
        json!({ "enabled": true }),
        RPC_TIMEOUT,
    );
    client.start_heartbeat(HEARTBEAT_INTERVAL);

    // Probe for live adapters once at startup. The user can `:attach <id>`
    // or `:attach all` to load them into local tabs without losing the
    // option to start with a clean slate. The same response also seeds
    // the `:attach <TAB>` completer cache so the operator can complete
    // server-side ids they don't have locally yet.
    let live_hint = match client.call("adapter.live", json!({}), RPC_TIMEOUT) {
        Ok(value) => {
            adapters_cache.set(extract_live_ids(&value));
            format_live_hint(&value)
        }
        Err(_) => None,
    };

    // Persistent history at `~/.ptywright/repl-history` (or wherever
    // `PTYWRIGHT_HOME` points).
    let history_path = Paths::from_env().repl_history_path();
    let history = super::history::open(&history_path)?;

    let completer = Box::new(ReplCompleter::new(
        Arc::clone(&ctx),
        plugins,
        adapters_cache,
    ));
    let highlighter = Box::new(ReplHighlighter::new());
    let hinter =
        Box::new(DefaultHinter::default().with_style(Style::new().italic().fg(Color::DarkGray)));

    // External printer relays `[notif] …` lines from a background thread
    // above the prompt without disturbing the line editor. reedline polls
    // the receiver between events when configured.
    //
    // The printer thread observes a shared stop flag so REPL teardown
    // does not leave it parked on `recv` forever — important on the
    // socket transport, where the server may not close the broadcast
    // channel promptly when we drop our writer half.
    let external_printer = ExternalPrinter::<String>::default();
    let notification_sender = external_printer.sender();
    let notifications_rx = client.notifications();
    let printer_stop = Arc::new(AtomicBool::new(false));
    let printer_stop_clone = Arc::clone(&printer_stop);
    // Live-pane refresh channel — bounded(1) coalesces a flurry of
    // session.changed notifications into one redraw. The notification
    // thread feeds it; the redraw thread drains it (with a 30 ms
    // debounce) and re-fetches the focused adapter's snapshot. The
    // main thread keeps a clone of the sender so it can kick refreshes
    // from command boundaries (`session.spawn`, `:focus`, etc.).
    let (refresh_tx, refresh_rx) = refresh_channel();
    let refresh_tx_for_printer: RefreshTx = refresh_tx.clone();
    let ctx_for_printer = Arc::clone(&ctx);
    std::thread::Builder::new()
        .name("ptywright-repl-notif-printer".into())
        .spawn(move || {
            use crossbeam_channel::RecvTimeoutError;
            while !printer_stop_clone.load(Ordering::Relaxed) {
                match notifications_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(notification) => {
                        // Push a refresh signal first — the live pane
                        // wants to repaint whether or not we also log
                        // a line.
                        {
                            let ctx = ctx_for_printer.lock().expect("repl ctx mutex");
                            let _ = handle_change_notification(
                                &notification,
                                &ctx,
                                &refresh_tx_for_printer,
                            );
                        }
                        let Some(line) = format_notification(&notification) else {
                            continue;
                        };
                        if notification_sender.send(line).is_err() {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .expect("spawn notification printer thread");

    // RAII guard so the printer thread is signaled regardless of which
    // exit path `run` takes (clean `:quit`, Ctrl-D, reedline error, or a
    // panic). Constructed immediately after the spawn so any panic in
    // the line-editor setup below still trips it on stack unwind.
    let _printer_guard = PrinterStopGuard(Arc::clone(&printer_stop));

    // Enter the alternate screen + DECSTBM scroll region for the live
    // pane. The guard restores the terminal on Drop, including on
    // panic — without it, an unwinding Ctrl-C would leave the operator
    // in a half-raw, half-alt-screen terminal.
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let layout = LiveLayout::compute(term_cols, term_rows);
    let alt_guard = AltScreenGuard::enter(&layout)
        .map_err(|error| Error::Rpc(format!("enter alt-screen: {error}")))?;
    let _alt_guard = alt_guard;

    // Live-pane paint coordination + background redraw thread.
    let paint_lock = Arc::new(PaintLock::default());
    let redraw_stop = Arc::new(AtomicBool::new(false));
    let _redraw_handle = spawn_redraw_thread(
        Arc::clone(&client),
        Arc::clone(&ctx),
        Arc::clone(&paint_lock),
        refresh_rx,
        Arc::clone(&redraw_stop),
    );
    let _redraw_stop_guard = RedrawStopGuard(Arc::clone(&redraw_stop));
    // Kick an initial paint so the empty-state hint ("no live sessions
    // — try session.spawn(...)") shows up before the operator even
    // types. The bounded(1) channel coalesces with subsequent refreshes
    // from real notifications, so worst case we double-paint once at
    // startup.
    let _ = refresh_tx.try_send(());

    // Register a columnar completion menu so Tab shows candidates and
    // Tab / Shift-Tab cycle through them.
    let menu = ReedlineMenu::EngineCompleter(Box::new(
        ColumnarMenu::default().with_name("completion_menu"),
    ));
    let mut keybinds = default_emacs_keybindings();
    keybinds.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybinds.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    let edit_mode = Box::new(Emacs::new(keybinds));

    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_highlighter(highlighter)
        .with_history(Box::new(history))
        .with_hinter(hinter)
        .with_menu(menu)
        .with_edit_mode(edit_mode)
        .with_external_printer(external_printer);

    let prompt = PtywrightPrompt {
        transport_label: transport_label.clone(),
    };

    // Banner. `nu-ansi-term` prints inline ANSI; the terminal is in
    // line-by-line mode so the codes do not interfere with reedline's
    // column accounting.
    println!(
        "{}  {}",
        Color::Cyan.bold().paint("ptywright repl"),
        Style::new().dimmed().paint(&transport_label),
    );
    println!(
        "{}",
        Style::new()
            .dimmed()
            .paint("Type :help for commands · :quit to exit · Tab/Shift-Tab cycles completions",),
    );
    if let Some(hint) = live_hint {
        println!("{}", Color::Yellow.paint(hint));
    }
    println!();

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let cmd = match parse(trimmed) {
                    Ok(cmd) => cmd,
                    Err(error) => {
                        print_error(&error.to_string());
                        continue;
                    }
                };
                let outcome = {
                    let mut ctx = ctx.lock().expect("repl ctx mutex");
                    super::command::dispatch(cmd, &client, &mut ctx, RPC_TIMEOUT)
                };
                match outcome {
                    Ok(CmdOutcome::Quit) => return Ok(()),
                    Ok(CmdOutcome::ShowHelp(text)) => print_help(&text),
                    Ok(CmdOutcome::Line(text)) => print_line_result(&text),
                    Ok(CmdOutcome::Json(value)) => print_json_result(&value),
                    Ok(CmdOutcome::Note { note, .. }) => print_note_result(&note),
                    Ok(CmdOutcome::Screen { adapter, snapshot }) => {
                        print_screen(&adapter, &snapshot)
                    }
                    Err(error) => print_error(&error.to_string()),
                }
                // Refresh the live pane after every command — most
                // commands shift state (focus, spawn, close,
                // send.text, etc.) that the tab strip or snapshot
                // would surface, and the bounded(1) channel keeps
                // back-to-back refreshes from queuing up.
                let _ = refresh_tx.try_send(());
            }
            Ok(Signal::CtrlC) => {
                println!("{}", Style::new().dimmed().paint("(input cancelled)"));
                continue;
            }
            Ok(Signal::CtrlD) => {
                println!("{}", Style::new().dimmed().paint("bye."));
                return Ok(());
            }
            // `Signal` is non-exhaustive — future reedline versions may
            // add new variants. Treat anything else as "keep going" so
            // adding a Signal doesn't break a release-build REPL.
            Ok(_) => continue,
            Err(error) => {
                return Err(Error::Rpc(format!("reedline error: {error}")));
            }
        }
    }
}

/// RAII helper that flips the notification-printer thread's stop flag
/// when this guard is dropped, so the printer cannot outlive a `run()`
/// invocation that returned via any exit path (clean quit, error, or
/// panic).
struct PrinterStopGuard(Arc<AtomicBool>);

impl Drop for PrinterStopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// RAII helper analogous to [`PrinterStopGuard`] for the live-pane
/// redraw thread. The redraw thread also keys on this flag, so a
/// panic during reedline setup or `read_line` still tears the thread
/// down cleanly.
struct RedrawStopGuard(Arc<AtomicBool>);

impl Drop for RedrawStopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn extract_plugin_names(value: &Value) -> Option<Vec<String>> {
    let plugins = value.get("plugins")?.as_array()?;
    Some(
        plugins
            .iter()
            .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
            .collect(),
    )
}

/// Extract running adapter ids from an `adapter.live` response, skipping
/// finished entries and malformed rows. Used to seed the completer cache
/// and to drive the banner hint formatter.
fn extract_live_ids(value: &Value) -> Vec<String> {
    let Some(adapters) = value.get("adapters").and_then(Value::as_array) else {
        return Vec::new();
    };
    adapters
        .iter()
        .filter(|entry| {
            !entry
                .get("finished")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            entry
                .get("adapter")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty())
        .collect()
}

/// Build the live-adapter hint shown under the banner. `None` when the
/// server reports no live adapters (don't waste a banner row).
fn format_live_hint(value: &Value) -> Option<String> {
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

/// Format a server notification into the dim line surfaced through the
/// reedline external printer, or `None` to drop the frame entirely.
///
/// The high-rate, low-signal notifications — `session.output` (raw VT
/// bytes that render as JSON-escaped garbage above the prompt) and
/// `session.changed` (one frame per PTY byte burst, no human-readable
/// payload) — are absorbed by the live screen pane instead of being
/// printed inline. `session.exited` and any future plugin-defined
/// notifications still surface here so the operator sees lifecycle
/// events and unknown methods rather than silently dropping them.
fn format_notification(notification: &Notification) -> Option<String> {
    match notification.method.as_str() {
        // Dropped: handled by the live screen pane redraw path. Printing
        // them inline buried the prompt in escape-byte noise and was the
        // single biggest UX complaint about the old REPL.
        "session.output" | "session.changed" => None,
        _ => Some(render_notification_line(notification)),
    }
}

/// Build the dim `[notif] …` line for notifications the operator should
/// see. Split out from [`format_notification`] so future-method handling
/// stays in one place and tests can exercise it directly.
fn render_notification_line(notification: &Notification) -> String {
    let session = notification
        .params
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let sequence = notification.params.get("sequence").and_then(Value::as_u64);
    let body = match (notification.method.as_str(), sequence) {
        ("session.exited", Some(seq)) => format!("session.exited  {session} seq={seq}"),
        ("session.exited", None) => format!("session.exited  {session}"),
        (other, _) => format!("{other} {}", notification.params),
    };
    format!(
        "{} {}",
        Style::new().dimmed().paint("[notif]"),
        Style::new().dimmed().paint(body),
    )
}

// ---- result printing ---------------------------------------------------

fn print_line_result(text: &str) {
    let mut first = true;
    for line in text.split('\n') {
        if first {
            println!("  {}", Style::new().dimmed().paint(format!("↳ {line}")));
            first = false;
        } else if !line.is_empty() {
            println!("    {}", Style::new().dimmed().paint(line));
        }
    }
}

fn print_json_result(value: &Value) {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    let max = 240;
    let truncated = if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text
    };
    println!(
        "  {} {}",
        Color::Cyan.paint("↳"),
        Style::new().dimmed().paint(truncated),
    );
}

/// Render a structured [`super::notes::Note`] under the prompt. The note
/// already knows how to paint itself — we just emit its line.
fn print_note_result(note: &super::notes::Note) {
    println!("{}", note.render());
}

fn print_help(text: &str) {
    println!();
    println!("{}", Color::Yellow.bold().paint("ptywright repl · help"));
    for line in text.lines() {
        // Lightly color section headers (lines ending in `:`) and indent
        // command rows so help is scannable even though we stream it as
        // plain stdout.
        if line.ends_with(':') {
            println!("{}", Color::Yellow.paint(line));
        } else if line.is_empty() {
            println!();
        } else {
            println!("{}", Style::new().dimmed().paint(line));
        }
    }
    println!();
}

fn print_error(message: &str) {
    println!(
        "  {} {}",
        Color::Red.bold().paint("✗"),
        Color::Red.paint(message),
    );
}

/// Render a `ScreenSnapshot` into the scrollback using ANSI codes so the
/// operator sees the PTY exactly as the agent would. Cells are walked in
/// row-major order, contiguous same-style runs are batched into one
/// paint() call to keep the byte stream compact.
fn print_screen(adapter: &str, snapshot: &ScreenSnapshot) {
    let dim = Style::new().dimmed();
    let title = format!(
        " preview · {adapter} · {}×{} · seq {} ",
        snapshot.size.cols, snapshot.size.rows, snapshot.sequence,
    );
    let rule_width = (snapshot.size.cols as usize).clamp(40, 120);
    let header = format!(
        "─{title:─^width$}─",
        title = title,
        width = rule_width.saturating_sub(2),
    );
    println!();
    println!("  {}", dim.paint(header));

    // Group cells by row and sort by column. The vt100 backend already
    // emits cells row-major but it's cheap insurance — and lets us tolerate
    // future backends that might not.
    let rows_count = snapshot.size.rows as usize;
    let mut rows: Vec<Vec<&_>> = (0..rows_count).map(|_| Vec::new()).collect();
    for cell in &snapshot.cells {
        let r = cell.row as usize;
        if r < rows.len() {
            rows[r].push(cell);
        }
    }
    for row in &mut rows {
        row.sort_by_key(|c| c.col);
    }

    for row in rows {
        print!("  ");
        let mut buffer = String::new();
        let mut current_style: Option<Style> = None;
        for cell in row {
            if cell.wide_continuation {
                continue;
            }
            let style = cell_style_to_ansi(&cell.style);
            let text = if cell.text.is_empty() {
                " "
            } else {
                cell.text.as_str()
            };
            match current_style {
                Some(s) if s == style => buffer.push_str(text),
                _ => {
                    if let Some(s) = current_style.take()
                        && !buffer.is_empty()
                    {
                        print!("{}", s.paint(std::mem::take(&mut buffer)));
                    }
                    current_style = Some(style);
                    buffer.push_str(text);
                }
            }
        }
        if let Some(s) = current_style
            && !buffer.is_empty()
        {
            print!("{}", s.paint(buffer));
        }
        println!();
    }

    let footer = format!(
        "─ end · cursor {}:{}{} ─",
        snapshot.cursor.row,
        snapshot.cursor.col,
        if snapshot.alternate_screen {
            " · alt-screen"
        } else {
            ""
        },
    );
    println!("  {}", dim.paint(footer));
    println!();
}

fn cell_style_to_ansi(style: &ScreenCellStyle) -> Style {
    let mut out = Style::new();
    if let Some(color) = parse_cell_color(&style.foreground) {
        out = out.fg(color);
    }
    if let Some(color) = parse_cell_color(&style.background) {
        out = out.on(color);
    }
    if style.bold {
        out = out.bold();
    }
    if style.dim {
        out = out.dimmed();
    }
    if style.italic {
        out = out.italic();
    }
    if style.underline {
        out = out.underline();
    }
    if style.inverse {
        out = out.reverse();
    }
    out
}

fn parse_cell_color(value: &str) -> Option<Color> {
    if value == "default" {
        return None;
    }
    if let Some(idx) = value.strip_prefix("idx:")
        && let Ok(n) = idx.parse::<u8>()
    {
        return Some(Color::Fixed(n));
    }
    if let Some(rgb) = value.strip_prefix("rgb:") {
        let parts: Vec<&str> = rgb.split(':').collect();
        if parts.len() == 3 {
            let r = parts[0].parse().ok()?;
            let g = parts[1].parse().ok()?;
            let b = parts[2].parse().ok()?;
            return Some(Color::Rgb(r, g, b));
        }
    }
    None
}

// ---- Prompt impl --------------------------------------------------------

struct PtywrightPrompt {
    transport_label: String,
}

impl Prompt for PtywrightPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(format!("{}", Color::Green.bold().paint("pty> ")))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(format!(
            "{}",
            Style::new().dimmed().paint(self.transport_label.clone()),
        ))
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  … ")
    }

    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("(history) ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn live_hint_skips_finished_adapters() {
        let value = json!({
            "adapters": [
                { "adapter": "e3", "plugin": "claude-code", "finished": true, "session": "s3", "sequence": 99 }
            ]
        });
        assert!(format_live_hint(&value).is_none());
    }

    #[test]
    fn session_changed_is_dropped_from_the_printer() {
        let notif = Notification {
            method: "session.changed".to_string(),
            params: json!({ "session": "s4", "sequence": 42 }),
        };
        assert!(
            format_notification(&notif).is_none(),
            "session.changed must not surface as an inline `[notif]` line; the live pane absorbs it",
        );
    }

    #[test]
    fn session_output_is_dropped_from_the_printer() {
        // The original bug: `session.output` carries raw VT bytes that
        // render as JSON-escaped garbage when printed inline. Confirm
        // the filter drops them regardless of payload content.
        let notif = Notification {
            method: "session.output".to_string(),
            params: json!({
                "session": "s1",
                "sequence": 6,
                "output": "\u{001b}[<\u{001b}[>1\u{001b}[>4;2m",
            }),
        };
        assert!(
            format_notification(&notif).is_none(),
            "session.output must never reach the printer — it's raw VT bytes",
        );
    }

    #[test]
    fn session_exited_still_prints() {
        let notif = Notification {
            method: "session.exited".to_string(),
            params: json!({ "session": "s1", "sequence": 9 }),
        };
        let line = format_notification(&notif).expect("session.exited prints");
        assert!(line.contains("session.exited"));
        assert!(line.contains("s1"));
        assert!(line.contains("seq=9"));
    }

    #[test]
    fn unknown_notification_methods_still_print() {
        // Forward-compat: plugins may emit their own notification
        // methods in the future. Drop them at the filter is wrong —
        // the operator should at least see they exist.
        let notif = Notification {
            method: "future.event".to_string(),
            params: json!({ "anything": "goes" }),
        };
        let line = format_notification(&notif).expect("unknown notifications still print");
        assert!(line.contains("future.event"));
    }
}
