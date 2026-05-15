//! Sequential reedline-based REPL.
//!
//! Each command is rendered as `pty> <syntax-highlighted DSL>` and the
//! result follows on the next line as `↳ <dim summary>`, matching the
//! original mockup. Line editing, completion, syntax highlighting,
//! history, and ghost-text hinting are all delegated to reedline; this
//! module only handles the read-eval-print loop, the prompt, and how
//! each `CmdOutcome` is printed.

use std::borrow::Cow;
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
use super::completer::{PluginCache, ReplCompleter};
use super::ctx::ReplCtx;
use super::highlighter::ReplHighlighter;
use super::transport::{Notification, RpcClient};
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::screen::{ScreenCellStyle, ScreenSnapshot};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the REPL pings the server to keep its notification pump warm.
/// The server only polls notifications on inbound requests, so a parked
/// prompt needs a heartbeat for `session.changed` events from other
/// connections to reach us. See `RpcClient::start_heartbeat`.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(300);

/// Public entry. Builds reedline + prompt and runs the read-eval-print
/// loop until the user quits with `:quit` / `Ctrl-D`.
pub fn run(client: Arc<RpcClient>, transport_label: String) -> Result<()> {
    let ctx = Arc::new(Mutex::new(ReplCtx::new()));
    let plugins = PluginCache::new();

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
    // option to start with a clean slate.
    let live_hint = match client.call("adapter.live", json!({}), RPC_TIMEOUT) {
        Ok(value) => format_live_hint(&value),
        Err(_) => None,
    };

    // Persistent history at `~/.ptywright/repl-history` (or wherever
    // `PTYWRIGHT_HOME` points).
    let history_path = Paths::from_env().repl_history_path();
    let history = super::history::open(&history_path)?;

    let completer = Box::new(ReplCompleter::new(Arc::clone(&ctx), plugins));
    let highlighter = Box::new(ReplHighlighter::new());
    let hinter =
        Box::new(DefaultHinter::default().with_style(Style::new().italic().fg(Color::DarkGray)));

    // External printer relays `[notif] …` lines from a background thread
    // above the prompt without disturbing the line editor. reedline polls
    // the receiver between events when configured.
    let external_printer = ExternalPrinter::<String>::default();
    let notification_sender = external_printer.sender();
    let notifications_rx = client.notifications();
    std::thread::Builder::new()
        .name("ptywright-repl-notif-printer".into())
        .spawn(move || {
            // Drop the printer's sender when the channel closes (i.e.,
            // RpcClient is being torn down) so the print queue does not
            // grow forever.
            while let Ok(notification) = notifications_rx.recv() {
                let line = format_notification(&notification);
                if notification_sender.send(line).is_err() {
                    return;
                }
            }
        })
        .expect("spawn notification printer thread");

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
                    Ok(CmdOutcome::Screen { adapter, snapshot }) => {
                        print_screen(&adapter, &snapshot)
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

fn extract_plugin_names(value: &Value) -> Option<Vec<String>> {
    let plugins = value.get("plugins")?.as_array()?;
    Some(
        plugins
            .iter()
            .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
            .collect(),
    )
}

/// Build the live-adapter hint shown under the banner. `None` when the
/// server reports no live adapters (don't waste a banner row).
fn format_live_hint(value: &Value) -> Option<String> {
    let adapters = value.get("adapters")?.as_array()?;
    let mut ids: Vec<String> = Vec::new();
    for entry in adapters {
        if entry
            .get("finished")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(id) = entry.get("adapter").and_then(Value::as_str) {
            ids.push(id.to_string());
        }
    }
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
/// reedline external printer.
fn format_notification(notification: &Notification) -> String {
    let session = notification
        .params
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let sequence = notification.params.get("sequence").and_then(Value::as_u64);
    let body = match (notification.method.as_str(), sequence) {
        ("session.changed", Some(seq)) => format!("session.changed {session} seq={seq}"),
        ("session.exited", Some(seq)) => format!("session.exited  {session} seq={seq}"),
        ("session.changed", None) => format!("session.changed {session}"),
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
        let line = format_notification(&notif);
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
        let line = format_notification(&notif);
        assert!(line.contains("future.event"));
    }
}
