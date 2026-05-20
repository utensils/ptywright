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

use super::completer::{AdapterCache, PluginCache, ReplCompleter};
use super::ctx::ReplCtx;
use super::highlighter::ReplHighlighter;
use super::lua::{LuaRepl, LuaValidator, RenderedValue};
use super::meta::{self, MetaOutcome};
use super::tips;
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

    // Embedded Lua evaluator — built early so completer/highlighter can
    // share its handle + globals whitelist.
    let lua_repl = LuaRepl::new(Arc::clone(&client), Arc::clone(&ctx), RPC_TIMEOUT)?;

    let completer = Box::new(ReplCompleter::new(
        Arc::clone(&ctx),
        plugins,
        adapters_cache,
        lua_repl.lua_handle(),
        lua_repl.repl_globals(),
    ));
    let highlighter = Box::new(ReplHighlighter::new(lua_repl.repl_globals()));
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
    std::thread::Builder::new()
        .name("ptywright-repl-notif-printer".into())
        .spawn(move || {
            use crossbeam_channel::RecvTimeoutError;
            while !printer_stop_clone.load(Ordering::Relaxed) {
                match notifications_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(notification) => {
                        if let Some(line) = format_notification(&notification)
                            && notification_sender.send(line).is_err()
                        {
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

    let validator = Box::new(LuaValidator::new(lua_repl.lua_handle()));
    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_highlighter(highlighter)
        .with_history(Box::new(history))
        .with_hinter(hinter)
        .with_menu(menu)
        .with_edit_mode(edit_mode)
        .with_validator(validator)
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
        "{} {}",
        Color::Yellow.paint("tip ·"),
        Style::new().italic().paint(tips::rotating_tip()),
    );
    println!(
        "{}",
        Style::new()
            .dimmed()
            .paint("Type :tips for a tour · :help for the command reference · :quit to exit"),
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
                // Lines starting with `:` are meta commands — they
                // manage REPL-local state (focus, notification
                // subscriptions, raw `:rpc`) and intentionally aren't
                // Lua-callable, so they bypass the Lua VM.
                if let Some(rest) = trimmed.strip_prefix(':') {
                    let outcome = {
                        let mut ctx = ctx.lock().expect("repl ctx mutex");
                        meta::dispatch(rest, &client, &mut ctx, RPC_TIMEOUT)
                    };
                    match outcome {
                        Ok(MetaOutcome::Quit) => return Ok(()),
                        Ok(MetaOutcome::ShowHelp(text)) => print_help(&text),
                        Ok(MetaOutcome::ShowTips(text)) => print_tips(&text),
                        Ok(MetaOutcome::Line(text)) => print_line_result(&text),
                        Ok(MetaOutcome::Json(value)) => print_json_result(&value),
                        Ok(MetaOutcome::Screen { adapter, snapshot }) => {
                            print_screen(&adapter, &snapshot)
                        }
                        Err(error) => print_error(&error.to_string()),
                    }
                    continue;
                }
                // Everything else is Lua. The full DSL surface
                // (session.*, send.*, wait.*, …) is bound as Lua
                // globals by `LuaRepl::new`.
                match lua_repl.eval(trimmed) {
                    Ok(result) => {
                        for value in result.values {
                            match value {
                                RenderedValue::Text(text) => print_line_result(&text),
                                RenderedValue::Screen { adapter, snapshot } => {
                                    print_screen(&adapter, &snapshot)
                                }
                            }
                        }
                    }
                    Err(error) => print_error(&error.to_string()),
                }
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
/// reedline external printer. Returns `None` for noisy / high-volume
/// notifications (`session.output`) that would dump raw VT escape bytes
/// into the prompt area — the operator can opt back in via the
/// `transcript.snapshot()` / `view()` bindings when they want to see PTY
/// content.
fn format_notification(notification: &Notification) -> Option<String> {
    let session = notification
        .params
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let sequence = notification.params.get("sequence").and_then(Value::as_u64);
    let body = match (notification.method.as_str(), sequence) {
        // session.output streams every PTY byte the server saw — the
        // JSON-escaped payload is raw VT escape codes that look like
        // garbage and bury the prompt. Drop it from the print path; the
        // transcript / screen views give a curated rendering instead.
        ("session.output", _) => return None,
        ("session.changed", Some(seq)) => format!("session.changed {session} seq={seq}"),
        ("session.exited", Some(seq)) => format!("session.exited  {session} seq={seq}"),
        ("session.changed", None) => format!("session.changed {session}"),
        ("session.exited", None) => format!("session.exited  {session}"),
        // Unknown methods — surface them but compact the params so an
        // unfamiliar future notification doesn't dump a multi-KB blob.
        (other, _) => {
            let params = compact_notification_params(&notification.params);
            format!("{other} {session} {params}")
        }
    };
    Some(format!(
        "{} {}",
        Style::new().dimmed().paint("[notif]"),
        Style::new().dimmed().paint(body),
    ))
}

/// Compact a notification's params object for display. Drops a few
/// well-known noisy keys (`output`, `text`, `data`) so unknown future
/// notifications don't leak their full payload onto the prompt line.
fn compact_notification_params(params: &Value) -> String {
    let mut summary = serde_json::Map::new();
    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "output" | "text" | "data") {
                continue;
            }
            summary.insert(k.clone(), v.clone());
        }
    }
    if summary.is_empty() {
        String::new()
    } else {
        serde_json::Value::Object(summary).to_string()
    }
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

fn print_help(text: &str) {
    print_section("ptywright repl · help", text);
}

/// `:tips` renders the curated tour with the same section-aware layout
/// as `:help`. The split lets each command carry its own title without
/// hard-coding either string in [`print_section`].
fn print_tips(text: &str) {
    print_section("ptywright repl · tips", text);
}

fn print_section(title: &str, text: &str) {
    println!();
    println!("{}", Color::Yellow.bold().paint(title));
    for line in text.lines() {
        // Lightly color section headers (lines ending in `:`) and indent
        // command rows so the section reads scannably even though we
        // stream it as plain stdout.
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
///
/// Trimming strategy:
///   * Vertical: blank rows above the first / below the last content row
///     are dropped (cursor row is the floor on both ends).
///   * Horizontal: each row stops at its rightmost non-blank cell so a
///     30-col line doesn't trail 170 cols of empty padding.
///   * Width clamp: the rendered slice is clamped to `min(content_width,
///     user_term_cols - leading_indent, pty_cols)` so a 200×60 PTY does
///     not wrap off the side of a narrower host terminal. Truncated
///     rows get a dim `…` marker so the operator sees that something
///     was elided.
fn print_screen(adapter: &str, snapshot: &ScreenSnapshot) {
    let dim = Style::new().dimmed();
    let rows_total = snapshot.size.rows as usize;
    let pty_cols = snapshot.size.cols as usize;

    // Group cells by row.
    let mut rows: Vec<Vec<&_>> = (0..rows_total).map(|_| Vec::new()).collect();
    for cell in &snapshot.cells {
        let r = cell.row as usize;
        if r < rows.len() {
            rows[r].push(cell);
        }
    }
    for row in &mut rows {
        row.sort_by_key(|c| c.col);
    }

    let range = visible_range(&rows, snapshot.cursor.row as usize);

    // Pre-compute each visible row's rightmost non-blank column (1-based)
    // so we can both pick the global rendering width and skip trailing
    // whitespace per row.
    let row_widths: Vec<usize> = rows[range.start..range.end]
        .iter()
        .map(|row| row_content_width(row))
        .collect();
    let content_width = row_widths.iter().copied().max().unwrap_or(0);
    let effective_width = effective_render_width(content_width, detect_term_cols(), pty_cols);

    let title = format!(
        " preview · {adapter} · {}×{} · seq {} ",
        snapshot.size.cols, snapshot.size.rows, snapshot.sequence,
    );
    let header = format!(
        "─{title:─^width$}─",
        title = title,
        width = effective_width.saturating_sub(2),
    );
    println!();
    println!("  {}", dim.paint(header));

    for (row, &row_width) in rows[range.start..range.end].iter().zip(row_widths.iter()) {
        print!("  ");
        let render_width = row_width.min(effective_width);
        let truncated = row_width > effective_width;
        let mut buffer = String::new();
        let mut current_style: Option<Style> = None;
        for cell in row {
            if cell.wide_continuation {
                continue;
            }
            let col = cell.col as usize;
            // Stop before a glyph that would spill past the clamp. A
            // wide cell occupies two columns, so a plain `col >=
            // render_width` test would still let a wide glyph starting
            // at `render_width - 1` print its second half out of bounds.
            let glyph_width = if cell.wide { 2 } else { 1 };
            if col + glyph_width > render_width {
                break;
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
        if truncated {
            print!("{}", dim.paint("…"));
        }
        println!();
    }

    let trimmed_note = format_trimmed_note(range.trimmed_top, range.trimmed_bottom);
    let footer = format!(
        "─ end · cursor {}:{}{}{} ─",
        snapshot.cursor.row,
        snapshot.cursor.col,
        if snapshot.alternate_screen {
            " · alt-screen"
        } else {
            ""
        },
        trimmed_note,
    );
    println!("  {}", dim.paint(footer));
    println!();
}

/// Row range to render plus how many blank rows were trimmed above and
/// below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleRange {
    start: usize,
    end: usize,
    trimmed_top: usize,
    trimmed_bottom: usize,
}

/// Decide which contiguous row slice to render. The trim is symmetric —
/// blank rows above the first content row are dropped along with blank
/// rows past the last content row — so a 60-row PTY whose content sits
/// in rows 6–18 paints 13 rows instead of being smothered by leading
/// padding. The cursor row is honoured as a floor on both ends so the
/// operator can always see where the PTY focus is, even if it sits in a
/// blank gutter.
fn visible_range(rows: &[Vec<&crate::screen::ScreenCell>], cursor_row: usize) -> VisibleRange {
    let total = rows.len();
    if total == 0 {
        return VisibleRange {
            start: 0,
            end: 0,
            trimmed_top: 0,
            trimmed_bottom: 0,
        };
    }
    let is_blank = |row: &Vec<&crate::screen::ScreenCell>| -> bool {
        row.iter().all(|cell| cell.text.trim().is_empty())
    };
    let first_content = rows.iter().position(|row| !is_blank(row));
    let last_content = rows.iter().rposition(|row| !is_blank(row));
    let cursor_row = cursor_row.min(total.saturating_sub(1));
    let (start, end_inclusive) = match (first_content, last_content) {
        (Some(f), Some(l)) => (f.min(cursor_row), l.max(cursor_row)),
        // Empty screen — anchor on the cursor so we always paint at
        // least one row. `trimmed_top` / `trimmed_bottom` are still
        // computed from `start` / `end`, so the footer accurately
        // reports the blank rows above and below that anchored row.
        _ => (cursor_row, cursor_row),
    };
    let end = (end_inclusive + 1).min(total);
    VisibleRange {
        start,
        end,
        trimmed_top: start,
        trimmed_bottom: total.saturating_sub(end),
    }
}

/// Compute the rightmost-non-blank-cell + cell-width for a row (in
/// other words, the smallest column count that still contains every
/// glyph in the row). Wide cells count as two columns; the continuation
/// half is ignored because it shares the visible glyph of its leader.
fn row_content_width(row: &[&crate::screen::ScreenCell]) -> usize {
    row.iter()
        .rev()
        .find(|cell| !cell.wide_continuation && !cell.text.trim().is_empty())
        .map(|cell| {
            let glyph_width = if cell.wide { 2 } else { 1 };
            cell.col as usize + glyph_width
        })
        .unwrap_or(0)
}

/// Pick the render width for the screen view. Picks the smallest of:
///
///   * `content_width` — the longest non-blank row (so we never paint
///     dead padding past the actual screen content).
///   * `term_cols - leading_indent` — the operator's terminal minus the
///     two-space indent the renderer prefixes onto every row.
///   * `pty_cols` — the PTY's own width (can't render more cells than
///     the PTY actually has).
///
/// Clamped to a 40-column floor so the header `─── preview ─── ` still
/// renders sensibly when the screen is mostly empty — but the floor is
/// itself capped at `pty_cols`, so a PTY narrower than 40 columns never
/// produces a rule wider than the PTY it describes.
fn effective_render_width(content_width: usize, term_cols: usize, pty_cols: usize) -> usize {
    const LEADING_INDENT: usize = 2;
    const MIN_WIDTH: usize = 40;
    let term_avail = term_cols.saturating_sub(LEADING_INDENT);
    let mut width = content_width;
    if term_avail > 0 {
        width = width.min(term_avail);
    }
    width = width.min(pty_cols);
    // The floor must never push the width back above `pty_cols`: a
    // 20-column PTY would otherwise yield a 40-column rule wider than
    // any row it could possibly contain.
    width.max(MIN_WIDTH.min(pty_cols))
}

/// Best-effort terminal width detection. Returns `0` when stdout is not
/// a TTY (piped to a file, captured by a test harness, etc.); callers
/// treat `0` as "no constraint".
fn detect_term_cols() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(0)
}

/// Render the footer's "N blank row(s) hidden" suffix. Reports both
/// directions separately when they differ so the operator knows whether
/// the screen padding is leading, trailing, or both.
fn format_trimmed_note(top: usize, bottom: usize) -> String {
    match (top, bottom) {
        (0, 0) => String::new(),
        (t, 0) => format!(" · {t} blank row(s) hidden above"),
        (0, b) => format!(" · {b} blank row(s) hidden below"),
        (t, b) => format!(" · {t} above / {b} below blank row(s) hidden"),
    }
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
    fn notification_line_includes_session_and_sequence() {
        let notif = Notification {
            method: "session.changed".to_string(),
            params: json!({ "session": "s4", "sequence": 42 }),
        };
        let line = format_notification(&notif).expect("session.changed should print");
        assert!(line.contains("session.changed"));
        assert!(line.contains("s4"));
        assert!(line.contains("seq=42"));
    }

    #[test]
    fn notification_line_falls_back_for_unknown_method() {
        let notif = Notification {
            method: "future.event".to_string(),
            params: json!({ "anything": "goes" }),
        };
        let line = format_notification(&notif).expect("unknown method should still print");
        assert!(line.contains("future.event"));
    }

    #[test]
    fn session_output_notification_is_dropped() {
        // session.output's payload is raw VT escape bytes (which look
        // like garbage when JSON-escaped). The printer thread must
        // never surface it as an inline `[notif]` line; only the
        // curated transcript / screen bindings should render PTY
        // content.
        let notif = Notification {
            method: "session.output".to_string(),
            params: json!({
                "session": "s1",
                "sequence": 7,
                "output": "\u{1b}[?25h\u{1b}[?2004h",
            }),
        };
        assert!(
            format_notification(&notif).is_none(),
            "session.output must be dropped from the [notif] printer path",
        );
    }

    #[test]
    fn compact_notification_params_drops_noisy_keys() {
        let value = json!({ "session": "s9", "output": "raw vt bytes here", "sequence": 3 });
        let summary = compact_notification_params(&value);
        assert!(!summary.contains("output"), "output key should be dropped");
        assert!(summary.contains("session"), "session key should remain");
        assert!(summary.contains("sequence"), "sequence should remain");
    }

    #[test]
    fn trimmed_note_renders_directionally() {
        assert_eq!(format_trimmed_note(0, 0), "");
        assert_eq!(format_trimmed_note(5, 0), " · 5 blank row(s) hidden above");
        assert_eq!(
            format_trimmed_note(0, 42),
            " · 42 blank row(s) hidden below"
        );
        assert_eq!(
            format_trimmed_note(5, 42),
            " · 5 above / 42 below blank row(s) hidden",
        );
    }

    /// Build a row layout of `total` rows where rows whose index appears
    /// in `content` carry a single non-blank cell. The cell metadata is
    /// the bare minimum `visible_range` needs to spot a non-blank row.
    fn rows_with_content(total: usize, content: &[usize]) -> Vec<Vec<crate::screen::ScreenCell>> {
        (0..total)
            .map(|r| {
                if content.contains(&r) {
                    vec![crate::screen::ScreenCell {
                        row: r as u16,
                        col: 0,
                        text: "x".to_string(),
                        wide: false,
                        wide_continuation: false,
                        style: crate::screen::ScreenCellStyle {
                            foreground: "default".to_string(),
                            background: "default".to_string(),
                            bold: false,
                            dim: false,
                            italic: false,
                            underline: false,
                            inverse: false,
                        },
                    }]
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    fn row_refs<'a>(
        owned: &'a [Vec<crate::screen::ScreenCell>],
    ) -> Vec<Vec<&'a crate::screen::ScreenCell>> {
        owned.iter().map(|r| r.iter().collect()).collect()
    }

    #[test]
    fn visible_range_trims_leading_and_trailing_blanks() {
        let owned = rows_with_content(60, &[6, 7, 12]);
        let rows = row_refs(&owned);
        let range = visible_range(&rows, 10);
        assert_eq!(range.start, 6);
        assert_eq!(range.end, 13);
        assert_eq!(range.trimmed_top, 6);
        assert_eq!(range.trimmed_bottom, 47);
    }

    #[test]
    fn visible_range_honours_cursor_above_content() {
        // Cursor at row 3 but the first content row is row 8 — the
        // cursor must stay inside the rendered slice so the operator can
        // see where the PTY focus is.
        let owned = rows_with_content(20, &[8, 9]);
        let rows = row_refs(&owned);
        let range = visible_range(&rows, 3);
        assert_eq!(range.start, 3, "cursor above content must extend the top");
        assert_eq!(range.end, 10);
    }

    #[test]
    fn visible_range_honours_cursor_below_content() {
        let owned = rows_with_content(20, &[2, 3]);
        let rows = row_refs(&owned);
        let range = visible_range(&rows, 15);
        assert_eq!(range.start, 2);
        assert_eq!(range.end, 16, "cursor below content must extend the bottom");
    }

    #[test]
    fn effective_render_width_clamps_to_smallest_constraint() {
        // Content fits, terminal narrower than content → terminal wins
        // (minus the 2-col indent).
        assert_eq!(effective_render_width(180, 100, 200), 98);
        // Content narrower than terminal → content wins.
        assert_eq!(effective_render_width(60, 200, 200), 60);
        // PTY narrower than both → PTY wins.
        assert_eq!(effective_render_width(300, 200, 80), 80);
        // 40-col floor kicks in even when content is empty.
        assert_eq!(effective_render_width(0, 200, 200), 40);
        // term_cols == 0 (non-TTY) → fall back to min(content, pty).
        assert_eq!(effective_render_width(80, 0, 200), 80);
        // The floor must not exceed a sub-40-col PTY — a 20-col PTY
        // yields at most a 20-col rule, never the bare 40-col floor.
        assert_eq!(effective_render_width(0, 200, 20), 20);
        assert_eq!(effective_render_width(100, 200, 20), 20);
    }

    #[test]
    fn row_content_width_returns_glyph_aware_right_edge() {
        let style = crate::screen::ScreenCellStyle {
            foreground: "default".into(),
            background: "default".into(),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        };
        let cells: Vec<crate::screen::ScreenCell> = vec![
            crate::screen::ScreenCell {
                row: 0,
                col: 0,
                text: "h".into(),
                wide: false,
                wide_continuation: false,
                style: style.clone(),
            },
            crate::screen::ScreenCell {
                row: 0,
                col: 1,
                text: "i".into(),
                wide: false,
                wide_continuation: false,
                style: style.clone(),
            },
            // trailing whitespace cells — these should not affect width.
            crate::screen::ScreenCell {
                row: 0,
                col: 2,
                text: " ".into(),
                wide: false,
                wide_continuation: false,
                style,
            },
        ];
        let refs: Vec<&crate::screen::ScreenCell> = cells.iter().collect();
        assert_eq!(row_content_width(&refs), 2);
    }

    #[test]
    fn row_content_width_counts_wide_cell_as_two_columns() {
        let style = crate::screen::ScreenCellStyle {
            foreground: "default".into(),
            background: "default".into(),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        };
        let cells: Vec<crate::screen::ScreenCell> = vec![crate::screen::ScreenCell {
            row: 0,
            col: 4,
            text: "漢".into(),
            wide: true,
            wide_continuation: false,
            style,
        }];
        let refs: Vec<&crate::screen::ScreenCell> = cells.iter().collect();
        assert_eq!(row_content_width(&refs), 6);
    }

    #[test]
    fn row_content_width_returns_zero_for_blank_row() {
        let refs: Vec<&crate::screen::ScreenCell> = Vec::new();
        assert_eq!(row_content_width(&refs), 0);
    }

    #[test]
    fn visible_range_handles_empty_screen() {
        let owned: Vec<Vec<crate::screen::ScreenCell>> = (0..10).map(|_| Vec::new()).collect();
        let rows = row_refs(&owned);
        let range = visible_range(&rows, 4);
        // Empty screen: anchor on the cursor so we always paint at least
        // one row. The trimmed counts are accurate — the footer will say
        // `4 above / 5 below`, which is what's actually hidden.
        assert_eq!(range.start, 4);
        assert_eq!(range.end, 5);
        assert_eq!(range.trimmed_top, 4);
        assert_eq!(range.trimmed_bottom, 5);
    }

    /// Build a real `ScreenSnapshot` by driving the vt100-backed
    /// `Terminal` with `bytes`. Lets the render smoke tests exercise
    /// `print_screen` against authentic cell / style / cursor data
    /// instead of a hand-assembled struct.
    fn snapshot_from(rows: u16, cols: u16, bytes: &[u8]) -> ScreenSnapshot {
        let mut term = crate::screen::Terminal::new(crate::target::TerminalSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        term.process(bytes);
        term.snapshot(1)
    }

    #[test]
    fn print_screen_renders_without_panicking() {
        // Smoke coverage for the render path: empty screen, a screen
        // with content, and a screen whose content overruns the clamp
        // (forcing the `…` truncation branch). The assertion is
        // "doesn't panic" — stdout content is exercised, not captured.
        print_screen("e1", &snapshot_from(10, 40, b""));
        print_screen("e1", &snapshot_from(10, 40, b"hello\r\nworld\r\n"));
        // A line far wider than the 40-col PTY → per-row trim + clamp.
        let wide = "x".repeat(200);
        print_screen("e1", &snapshot_from(10, 40, wide.as_bytes()));
        // Styled content exercises cell_style_to_ansi / parse_cell_color.
        print_screen(
            "e1",
            &snapshot_from(8, 30, b"\x1b[1;31mbold-red\x1b[0m plain\r\n"),
        );
    }

    #[test]
    fn print_helpers_render_without_panicking() {
        // The `print_*` helpers are pure stdout writers; calling them
        // here covers the formatting branches (section headers, blank
        // lines, multi-line indentation, JSON truncation).
        print_help(crate::repl::meta::help_text());
        print_tips(crate::repl::tips::long_guide());
        print_error("something went wrong");
        print_line_result("single line");
        print_line_result("first line\nsecond line\n\nfourth line");
        print_json_result(&json!({ "ok": true, "n": 7 }));
        // A payload longer than the 240-char cap exercises truncation.
        let big = json!({ "blob": "z".repeat(400) });
        print_json_result(&big);
    }
}
