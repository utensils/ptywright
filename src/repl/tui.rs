//! ratatui-driven event loop for `ptywright repl`.
//!
//! We do not call `Reedline::read_line`. Reedline's top-level loop wants to
//! own the terminal, which fights ratatui's full-screen mode. Instead we
//! drive our own minimal line editor on top of crossterm key events and
//! plug the existing `reedline::Completer` and `reedline::Highlighter`
//! impls in for tab completion and syntax highlighting. Persistent history
//! lives behind `reedline::FileBackedHistory` but is navigated with our
//! own up/down index — enough for v1 without dragging in reedline's full
//! prompt+keybinding stack.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use reedline::{Completer, Highlighter, Span as ReedSpan, StyledText};
use serde_json::Value;

use super::command::{Cmd, CmdOutcome, MetaCmd};
use super::completer::{PluginCache, ReplCompleter};
use super::ctx::{HistoryEntry, OutcomeKind, ReplCtx};
use super::highlighter::ReplHighlighter;
use super::render::{render_history, render_snapshot, render_tabs};
use super::snapshot::{self, SnapshotStore};
use super::transport::RpcClient;
use crate::error::{Error, Result};
use crate::paths::Paths;

const EVENT_POLL: Duration = Duration::from_millis(50);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const HISTORY_CAPACITY: usize = 200;

/// One clickable region tracked during render and consulted on mouse-down.
#[derive(Debug, Clone)]
struct Hotspot {
    rect: Rect,
    action: HotspotAction,
}

#[derive(Debug, Clone)]
enum HotspotAction {
    Focus(String),
    OpenHelp,
    Quit,
    ToggleNotifications,
}

/// Modal overlay rendered over the main area, dismissed by any key/click.
#[derive(Debug, Clone)]
enum Modal {
    Help(String),
}

// ---- platform-aware hotkey glyphs --------------------------------------
//
// macOS terminal users expect ⌃ / ⌥ / ⌫ glyphs; on Linux/Windows the
// "Ctrl-" / "Alt-" spellings are clearer. The actual keybindings are
// identical — terminals send the same byte sequences either way.

#[cfg(target_os = "macos")]
const KEY_CTRL: &str = "⌃";
#[cfg(not(target_os = "macos"))]
const KEY_CTRL: &str = "Ctrl-";

#[cfg(target_os = "macos")]
const KEY_ALT: &str = "⌥";
#[cfg(not(target_os = "macos"))]
const KEY_ALT: &str = "Alt-";

/// Public entry: build state and run the event loop until the user quits.
pub fn run(client: Arc<RpcClient>, transport_label: String) -> Result<()> {
    let mut ctx = ReplCtx::new();
    // Bootstrap a welcome line so first-run users see how to discover the
    // command surface without having to guess.
    ctx.record(crate::repl::ctx::HistoryEntry {
        input: "welcome".into(),
        kind: OutcomeKind::Note,
        detail: Some(
            "Type :help for commands · :quit to exit · click the footer hints with your mouse"
                .into(),
        ),
    });
    let ctx = Arc::new(Mutex::new(ctx));
    let store = Arc::new(SnapshotStore::new());
    let (redraw_tx, redraw_rx) = bounded::<()>(1);

    // Persistent command history: load past sessions from disk so Up/Down
    // recall survives across REPL invocations. Path is
    // `~/.ptywright/repl-history` (or whatever `PTYWRIGHT_HOME` points to).
    let history_path = Paths::from_env().repl_history_path();

    // Seed the plugin cache so completion has something useful immediately.
    let plugins = PluginCache::new();
    if let Ok(value) = client.call("adapter.list", serde_json::json!({}), RPC_TIMEOUT)
        && let Some(plugin_names) = extract_plugin_names(&value)
    {
        plugins.set(plugin_names);
    }

    // Best-effort subscribe — the server may not honor it, the pump's idle
    // backstop will keep things fresh either way.
    let mut notifications_enabled = match client.call(
        "server.set_notifications",
        serde_json::json!({ "enabled": true }),
        RPC_TIMEOUT,
    ) {
        Ok(value) => value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        Err(_) => false,
    };

    let _pump = snapshot::spawn(
        Arc::clone(&client),
        Arc::clone(&ctx),
        Arc::clone(&store),
        redraw_tx,
    );

    let mut terminal = ratatui::try_init()
        .map_err(|error| Error::Rpc(format!("initialise terminal for repl: {error}")))?;
    // Enable mouse capture so the tab strip + footer hints are clickable.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);

    let result = event_loop(
        &mut terminal,
        EventLoopArgs {
            client: Arc::clone(&client),
            ctx: Arc::clone(&ctx),
            store: Arc::clone(&store),
            redraw_rx,
            transport_label,
            plugins: plugins.clone(),
            completer: ReplCompleter::new(Arc::clone(&ctx), plugins),
            highlighter: ReplHighlighter::new(),
            notifications_enabled: &mut notifications_enabled,
            history_path,
        },
    );

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    let _ = ratatui::try_restore();
    result
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

struct EventLoopArgs<'a> {
    client: Arc<RpcClient>,
    ctx: Arc<Mutex<ReplCtx>>,
    store: Arc<SnapshotStore>,
    redraw_rx: Receiver<()>,
    transport_label: String,
    #[allow(dead_code)]
    plugins: PluginCache,
    completer: ReplCompleter,
    highlighter: ReplHighlighter,
    notifications_enabled: &'a mut bool,
    history_path: PathBuf,
}

fn event_loop(terminal: &mut DefaultTerminal, mut args: EventLoopArgs<'_>) -> Result<()> {
    let mut editor = LineEditor::new().with_persistent_history(args.history_path.clone());
    // Hotspots are rebuilt every frame so a resize never leaves stale rects.
    let hotspots: Arc<Mutex<Vec<Hotspot>>> = Arc::new(Mutex::new(Vec::new()));
    loop {
        let hotspots_clone = Arc::clone(&hotspots);
        terminal
            .draw(|frame| {
                let mut frame_hotspots = Vec::new();
                render(
                    frame,
                    &args.transport_label,
                    &args.ctx,
                    &args.store,
                    &editor,
                    &args.highlighter,
                    *args.notifications_enabled,
                    &mut frame_hotspots,
                );
                if let Ok(mut guard) = hotspots_clone.lock() {
                    *guard = frame_hotspots;
                }
            })
            .map_err(|error| Error::Rpc(format!("draw frame: {error}")))?;

        let event = poll_event(&args.redraw_rx)?;
        if let Some(event) = event {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Any key dismisses a modal — return Continue so the
                    // user can immediately resume typing without the
                    // keystroke being re-interpreted as a command.
                    if editor.modal.is_some() {
                        editor.modal = None;
                        continue;
                    }
                    match handle_key(key, &mut editor, &mut args) {
                        ControlFlow::Continue => {}
                        ControlFlow::Quit => return Ok(()),
                        ControlFlow::SwitchFocus(adapter) => {
                            if let Ok(mut ctx) = args.ctx.lock()
                                && ctx.adapter(&adapter).is_some()
                            {
                                ctx.focus = Some(adapter);
                            }
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    let hotspots_snap = hotspots.lock().ok().map(|g| g.clone()).unwrap_or_default();
                    match handle_mouse(mouse, &mut editor, &mut args, &hotspots_snap) {
                        ControlFlow::Continue => {}
                        ControlFlow::Quit => return Ok(()),
                        ControlFlow::SwitchFocus(adapter) => {
                            if let Ok(mut ctx) = args.ctx.lock()
                                && ctx.adapter(&adapter).is_some()
                            {
                                ctx.focus = Some(adapter);
                            }
                        }
                    }
                }
                Event::Resize(_, _) => { /* redraw covers it */ }
                _ => {}
            }
        }
    }
}

fn poll_event(redraw_rx: &Receiver<()>) -> Result<Option<Event>> {
    // Either a terminal event or a redraw tick will wake the loop.
    let has_event = event::poll(EVENT_POLL)
        .map_err(|error| Error::Rpc(format!("poll terminal events: {error}")))?;
    if has_event {
        return event::read()
            .map(Some)
            .map_err(|error| Error::Rpc(format!("read terminal event: {error}")));
    }
    // Drain any queued redraw ticks so we coalesce them into one frame.
    while redraw_rx.try_recv().is_ok() {}
    Ok(None)
}

// ---- line editor -------------------------------------------------------

struct LineEditor {
    buffer: String,
    cursor: usize,
    history: VecDeque<String>,
    history_idx: Option<usize>,
    stash: Option<String>,
    completions: Vec<String>,
    completion_idx: usize,
    completion_origin: Option<(ReedSpan, String)>,
    status: Option<(String, Style)>,
    modal: Option<Modal>,
    history_file: Option<PathBuf>,
}

impl LineEditor {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_idx: None,
            stash: None,
            completions: Vec::new(),
            completion_idx: 0,
            completion_origin: None,
            status: None,
            modal: None,
            history_file: None,
        }
    }

    /// Open `path` as the persistent history file. Loads existing entries
    /// (capped at `HISTORY_CAPACITY`) into the in-memory deque and
    /// remembers the path so new entries are appended on `record()`.
    fn with_persistent_history(mut self, path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines().rev().take(HISTORY_CAPACITY) {
                let line = line.trim();
                if !line.is_empty() {
                    self.history.push_front(line.to_string());
                }
            }
        }
        self.history_file = Some(path);
        self
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.completions.clear();
        self.completion_idx = 0;
        self.completion_origin = None;
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            // Step one Unicode scalar to the left, not one byte.
            self.cursor = prev_char_boundary(&self.buffer, self.cursor);
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = next_char_boundary(&self.buffer, self.cursor);
        }
    }

    fn insert(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.reset_completion();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.buffer, self.cursor);
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.reset_completion();
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.buffer.len();
    }

    fn record(&mut self, line: String) {
        if self.history.back().map(|s| s.as_str()) != Some(line.as_str()) {
            if self.history.len() >= HISTORY_CAPACITY {
                self.history.pop_front();
            }
            // Persist before pushing so a failed write does not poison the
            // in-memory deque. The file IO is best-effort: a permission or
            // disk-full error logs to tracing and the REPL keeps running.
            if let Some(path) = &self.history_file {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                {
                    let _ = writeln!(file, "{line}");
                } else {
                    tracing::debug!(
                        path = %path.display(),
                        "ptywright repl: could not append to history file"
                    );
                }
            }
            self.history.push_back(line);
        }
        self.history_idx = None;
        self.stash = None;
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_idx = match self.history_idx {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        if self.history_idx.is_none() {
            self.stash = Some(self.buffer.clone());
        }
        self.history_idx = Some(next_idx);
        self.buffer = self.history[next_idx].clone();
        self.cursor = self.buffer.len();
        self.reset_completion();
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_idx else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.buffer = self.stash.take().unwrap_or_default();
        } else {
            let next_idx = idx + 1;
            self.history_idx = Some(next_idx);
            self.buffer = self.history[next_idx].clone();
        }
        self.cursor = self.buffer.len();
        self.reset_completion();
    }

    fn cycle_completion(&mut self, completer: &mut ReplCompleter) {
        if self.completions.is_empty() {
            // First Tab: query the completer and stash the origin span so
            // subsequent Tabs cycle through alternatives in place rather
            // than appending alongside the previous pick.
            let suggestions = completer.complete(&self.buffer, self.cursor);
            if suggestions.is_empty() {
                return;
            }
            let span = suggestions[0].span;
            let base_text = self.buffer
                [span.start.min(self.buffer.len())..span.end.min(self.buffer.len())]
                .to_string();
            self.completion_origin = Some((span, base_text));
            self.completions = suggestions.into_iter().map(|s| s.value).collect();
            self.completion_idx = 0;
        } else {
            self.completion_idx = (self.completion_idx + 1) % self.completions.len();
        }
        let Some((span, _)) = self.completion_origin else {
            return;
        };
        let pick = self.completions[self.completion_idx].clone();
        let pick_len = pick.len();
        self.replace_span(span, &pick);
        // The replaced region now spans the full length of the new pick —
        // remember that so the *next* Tab replaces the just-inserted text
        // instead of inserting alongside it. (This was the bug behind the
        // "holding tab does wild completions" report.)
        self.completion_origin = Some((
            ReedSpan::new(span.start, span.start + pick_len),
            pick.clone(),
        ));
    }

    fn reset_completion(&mut self) {
        self.completions.clear();
        self.completion_idx = 0;
        self.completion_origin = None;
    }

    fn replace_span(&mut self, span: ReedSpan, replacement: &str) {
        let start = span.start.min(self.buffer.len());
        let end = span.end.min(self.buffer.len());
        self.buffer.replace_range(start..end, replacement);
        self.cursor = start + replacement.len();
    }

    fn set_status(&mut self, message: impl Into<String>, style: Style) {
        self.status = Some((message.into(), style));
    }

    fn clear_status(&mut self) {
        self.status = None;
    }
}

fn prev_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    idx -= 1;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    idx += 1;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

// ---- key handling ------------------------------------------------------

enum ControlFlow {
    Continue,
    Quit,
    SwitchFocus(String),
}

fn handle_key(key: KeyEvent, editor: &mut LineEditor, args: &mut EventLoopArgs<'_>) -> ControlFlow {
    editor.clear_status();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Alt+1..9 → focus that adapter tab.
    if alt
        && let KeyCode::Char(c) = key.code
        && let Some(n) = c.to_digit(10)
        && (1..=9).contains(&n)
    {
        let ids: Vec<String> = match args.ctx.lock() {
            Ok(ctx) => ctx.adapters.iter().map(|t| t.id.clone()).collect(),
            Err(_) => Vec::new(),
        };
        if let Some(id) = ids.get((n as usize).saturating_sub(1)) {
            return ControlFlow::SwitchFocus(id.clone());
        }
    }

    match key.code {
        KeyCode::Char('c') if ctrl => {
            // Ctrl-C: cancel current input. Doesn't terminate the REPL.
            editor.clear();
            editor.set_status("input cleared", Style::default().fg(Color::Yellow));
        }
        KeyCode::Char('d') if ctrl && editor.buffer.is_empty() => return ControlFlow::Quit,
        KeyCode::Char('l') if ctrl => {
            // Ctrl-L: clear history pane.
            if let Ok(mut ctx) = args.ctx.lock() {
                ctx.history.clear();
            }
        }
        KeyCode::Char(c) if !ctrl && !alt => editor.insert(c),
        KeyCode::Char(c) if shift && !ctrl && !alt => editor.insert(c),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Left => editor.move_left(),
        KeyCode::Right => editor.move_right(),
        KeyCode::Home => editor.home(),
        KeyCode::End => editor.end(),
        KeyCode::Up => editor.history_prev(),
        KeyCode::Down => editor.history_next(),
        KeyCode::Tab => editor.cycle_completion(&mut args.completer),
        KeyCode::Esc => editor.clear(),
        KeyCode::Enter => {
            let line = editor.buffer.trim().to_string();
            if line.is_empty() {
                return ControlFlow::Continue;
            }
            editor.record(line.clone());
            editor.clear();
            return execute_line(&line, args, editor);
        }
        _ => {}
    }
    ControlFlow::Continue
}

/// Translate a mouse event into a ControlFlow action. Modal-aware:
/// any click while the help popup is up dismisses it without firing the
/// underlying hotspot.
fn handle_mouse(
    mouse: MouseEvent,
    editor: &mut LineEditor,
    args: &mut EventLoopArgs<'_>,
    hotspots: &[Hotspot],
) -> ControlFlow {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return ControlFlow::Continue;
    }
    if editor.modal.is_some() {
        editor.modal = None;
        return ControlFlow::Continue;
    }
    let x = mouse.column;
    let y = mouse.row;
    for hotspot in hotspots {
        let r = hotspot.rect;
        if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
            return apply_hotspot(hotspot.action.clone(), editor, args);
        }
    }
    ControlFlow::Continue
}

fn apply_hotspot(
    action: HotspotAction,
    editor: &mut LineEditor,
    args: &mut EventLoopArgs<'_>,
) -> ControlFlow {
    match action {
        HotspotAction::Focus(id) => ControlFlow::SwitchFocus(id),
        HotspotAction::OpenHelp => {
            editor.modal = Some(Modal::Help(super::command::help_text().to_string()));
            ControlFlow::Continue
        }
        HotspotAction::Quit => ControlFlow::Quit,
        HotspotAction::ToggleNotifications => {
            let next = !*args.notifications_enabled;
            let _ = args.client.call(
                "server.set_notifications",
                serde_json::json!({ "enabled": next }),
                RPC_TIMEOUT,
            );
            *args.notifications_enabled = next;
            ControlFlow::Continue
        }
    }
}

fn execute_line(line: &str, args: &mut EventLoopArgs<'_>, editor: &mut LineEditor) -> ControlFlow {
    let parsed = match super::command::parse(line) {
        Ok(cmd) => cmd,
        Err(error) => {
            push_history(
                args,
                HistoryEntry {
                    input: line.to_string(),
                    kind: OutcomeKind::Error,
                    detail: Some(error.to_string()),
                },
            );
            return ControlFlow::Continue;
        }
    };

    // Detect notification toggles so we can update the footer's "notif" pill.
    let notification_toggle = match &parsed {
        Cmd::Meta(MetaCmd::Notifications(enabled)) => Some(*enabled),
        _ => None,
    };

    let outcome = {
        let mut ctx = match args.ctx.lock() {
            Ok(ctx) => ctx,
            Err(_) => {
                editor.set_status("ctx poisoned", Style::default().fg(Color::Red));
                return ControlFlow::Continue;
            }
        };
        super::command::dispatch(parsed, &args.client, &mut ctx, RPC_TIMEOUT)
    };

    match outcome {
        Ok(CmdOutcome::Quit) => ControlFlow::Quit,
        Ok(CmdOutcome::ShowHelp(text)) => {
            editor.modal = Some(Modal::Help(text));
            push_history(
                args,
                HistoryEntry {
                    input: line.to_string(),
                    kind: OutcomeKind::Note,
                    detail: Some("(help shown — press any key or click to dismiss)".into()),
                },
            );
            ControlFlow::Continue
        }
        Ok(CmdOutcome::Line(text)) => {
            push_history(
                args,
                HistoryEntry {
                    input: line.to_string(),
                    kind: OutcomeKind::Ok,
                    detail: Some(text),
                },
            );
            if let Some(enabled) = notification_toggle {
                *args.notifications_enabled = enabled;
            }
            ControlFlow::Continue
        }
        Ok(CmdOutcome::Json(value)) => {
            let summary = summarize_json(&value);
            push_history(
                args,
                HistoryEntry {
                    input: line.to_string(),
                    kind: OutcomeKind::Json,
                    detail: Some(summary),
                },
            );
            if let Some(enabled) = notification_toggle {
                *args.notifications_enabled = enabled;
            }
            ControlFlow::Continue
        }
        Err(error) => {
            push_history(
                args,
                HistoryEntry {
                    input: line.to_string(),
                    kind: OutcomeKind::Error,
                    detail: Some(error.to_string()),
                },
            );
            ControlFlow::Continue
        }
    }
}

fn summarize_json(value: &Value) -> String {
    // Single-line summary for the history pane. Full text is not surfaced
    // in the chrome — users can `:rpc <method>` and read responses in the
    // pretty-printed details popup (follow-up). For v1 we display the
    // first ~120 chars of a compact JSON encoding.
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    if text.chars().count() <= 120 {
        text
    } else {
        format!("{}…", text.chars().take(120).collect::<String>())
    }
}

fn push_history(args: &EventLoopArgs<'_>, entry: HistoryEntry) {
    if let Ok(mut ctx) = args.ctx.lock() {
        ctx.record(entry);
    }
}

// ---- rendering ---------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame<'_>,
    transport_label: &str,
    ctx: &Arc<Mutex<ReplCtx>>,
    store: &Arc<SnapshotStore>,
    editor: &LineEditor,
    highlighter: &ReplHighlighter,
    notifications_enabled: bool,
    hotspots: &mut Vec<Hotspot>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // tab strip
            Constraint::Min(8),    // main split (preview + history)
            Constraint::Length(3), // input
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    frame.render_widget(header_widget(transport_label), chunks[0]);

    let ctx_snapshot = ctx.lock().ok().map(|guard| guard.clone());
    render_tab_strip(frame, chunks[1], ctx_snapshot.as_ref(), hotspots);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(chunks[2]);

    render_preview_pane(frame, main[0], ctx_snapshot.as_ref(), store);
    render_history_pane(frame, main[1], ctx_snapshot.as_ref());

    render_input(frame, chunks[3], editor, highlighter);

    render_footer(
        frame,
        chunks[4],
        ctx_snapshot.as_ref(),
        editor,
        notifications_enabled,
        hotspots,
    );

    // Modal popup is drawn last so it overlays everything else.
    if let Some(modal) = &editor.modal {
        render_modal(frame, frame.area(), modal);
    }
}

/// Render the tab strip and append a `Focus(<id>)` hotspot per tab.
fn render_tab_strip(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    ctx: Option<&ReplCtx>,
    hotspots: &mut Vec<Hotspot>,
) {
    let line = ctx.map(render_tabs).unwrap_or_else(|| Line::from(""));
    frame.render_widget(Paragraph::new(line), area);
    let Some(ctx) = ctx else {
        return;
    };
    // Walk the same Span sequence `render_tabs` produces so the clickable
    // rects line up exactly with what the user sees.
    let mut col = area.x;
    for (idx, tab) in ctx.adapters.iter().enumerate() {
        if idx > 0 {
            col = col.saturating_add(2); // two-space separator
        }
        let state = tab.state_label.as_deref().unwrap_or("ready");
        let label = format!("[{}: {} · {}]", tab.id, tab.plugin, state);
        let width = label.chars().count() as u16;
        if col + width > area.x + area.width {
            break;
        }
        hotspots.push(Hotspot {
            rect: Rect::new(col, area.y, width, area.height.max(1)),
            action: HotspotAction::Focus(tab.id.clone()),
        });
        col = col.saturating_add(width);
    }
}

fn render_modal(frame: &mut ratatui::Frame<'_>, full: Rect, modal: &Modal) {
    let Modal::Help(body) = modal;
    // Center a 70x18 area (or as much as fits) and draw a bordered popup.
    let target_w = full.width.clamp(40, 90);
    let target_h = full.height.clamp(10, 22);
    let x = full.x + (full.width.saturating_sub(target_w)) / 2;
    let y = full.y + (full.height.saturating_sub(target_h)) / 2;
    let rect = Rect::new(x, y, target_w, target_h);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help · any key / click to dismiss ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(body.clone()).wrap(Wrap { trim: false }),
        inner,
    );
}

fn header_widget(transport_label: &str) -> Paragraph<'static> {
    let header = Line::from(vec![
        Span::styled(
            "ptywright repl",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::styled(
            transport_label.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    Paragraph::new(header)
}

fn render_preview_pane(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    ctx: Option<&ReplCtx>,
    store: &Arc<SnapshotStore>,
) {
    let title = match ctx.and_then(|c| c.focus.as_deref()) {
        Some(adapter) => format!(" preview · {adapter} "),
        None => " preview ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let body: Text<'static> = match ctx.and_then(|c| c.focus.as_deref()) {
        Some(adapter) => match store.get(adapter) {
            Some(snapshot) => render_snapshot(&snapshot),
            None => Text::from(Line::from(Span::styled(
                "(waiting for first snapshot…)".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ))),
        },
        None => Text::from(Line::from(Span::styled(
            "(no focused adapter)".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ))),
    };
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
}

fn render_history_pane(frame: &mut ratatui::Frame<'_>, area: Rect, ctx: Option<&ReplCtx>) {
    let block = Block::default().borders(Borders::ALL).title(" history ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let body = match ctx {
        Some(ctx) if !ctx.history.is_empty() => render_history(ctx),
        _ => Text::from(Line::from(Span::styled(
            "(type a command…)".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ))),
    };
    let total_lines = body.lines.len();
    let visible = inner.height as usize;
    let scroll = total_lines.saturating_sub(visible);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        inner,
    );
}

fn render_input(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    editor: &LineEditor,
    highlighter: &ReplHighlighter,
) {
    let block = Block::default().borders(Borders::ALL).title(" input ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let styled = highlighter.highlight(&editor.buffer, editor.cursor);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "❯ ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend(styled_text_to_spans(&styled));
    let mut lines = vec![Line::from(spans)];
    if let Some((status, style)) = &editor.status {
        lines.push(Line::from(Span::styled(status.clone(), *style)));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    // Place the visible cursor after the prompt + buffer prefix.
    let cursor_x = "❯ ".chars().count()
        + editor.buffer[..editor.cursor.min(editor.buffer.len())]
            .chars()
            .count();
    frame.set_cursor_position((inner.x + cursor_x as u16, inner.y));
}

/// Render the footer line and register click hotspots for `:help`,
/// `:quit`, and the notification pill.
fn render_footer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    ctx: Option<&ReplCtx>,
    editor: &LineEditor,
    notifications_enabled: bool,
    hotspots: &mut Vec<Hotspot>,
) {
    let focus = ctx
        .and_then(|c| c.focus.clone())
        .unwrap_or_else(|| "—".to_string());
    let cmd_count = ctx.map(|c| c.history.len()).unwrap_or_default();
    let notif_label = if notifications_enabled {
        "notif on".to_string()
    } else {
        "notif off".to_string()
    };
    let history_len = editor.history.len();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let link = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    // Build the spans and track each clickable label's column range so we
    // can register a hotspot covering exactly its rendered cells.
    let alt = KEY_ALT;
    let ctrl = KEY_CTRL;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut col = area.x;
    let push =
        |spans: &mut Vec<Span<'static>>, col: &mut u16, text: String, style: Style| -> (u16, u16) {
            let start = *col;
            let width = text.chars().count() as u16;
            spans.push(Span::styled(text, style));
            *col = col.saturating_add(width);
            (start, width)
        };

    push(
        &mut spans,
        &mut col,
        focus,
        Style::default().fg(Color::Cyan),
    );
    push(&mut spans, &mut col, "  ·  ".to_string(), dim);
    let (notif_x, notif_w) = push(&mut spans, &mut col, notif_label, link);
    hotspots.push(Hotspot {
        rect: Rect::new(notif_x, area.y, notif_w, 1),
        action: HotspotAction::ToggleNotifications,
    });
    push(&mut spans, &mut col, "  ·  ".to_string(), dim);
    push(&mut spans, &mut col, format!("{cmd_count} entries"), dim);
    push(&mut spans, &mut col, "  ·  ".to_string(), dim);
    push(&mut spans, &mut col, format!("{history_len} recalled"), dim);
    push(&mut spans, &mut col, "  ·  ".to_string(), dim);
    let (help_x, help_w) = push(&mut spans, &mut col, ":help".to_string(), link);
    hotspots.push(Hotspot {
        rect: Rect::new(help_x, area.y, help_w, 1),
        action: HotspotAction::OpenHelp,
    });
    push(
        &mut spans,
        &mut col,
        format!("  ·  {alt}1..9 focus · {ctrl}C clear · "),
        dim,
    );
    let (quit_x, quit_w) = push(&mut spans, &mut col, ":quit".to_string(), link);
    hotspots.push(Hotspot {
        rect: Rect::new(quit_x, area.y, quit_w, 1),
        action: HotspotAction::Quit,
    });

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn styled_text_to_spans(styled: &StyledText) -> Vec<Span<'static>> {
    styled
        .buffer
        .iter()
        .map(|(style, fragment)| {
            let mut out = Style::default();
            if let Some(color) = style.foreground.and_then(nu_color_to_ratatui) {
                out = out.fg(color);
            }
            if let Some(color) = style.background.and_then(nu_color_to_ratatui) {
                out = out.bg(color);
            }
            if style.is_bold {
                out = out.add_modifier(Modifier::BOLD);
            }
            if style.is_dimmed {
                out = out.add_modifier(Modifier::DIM);
            }
            if style.is_italic {
                out = out.add_modifier(Modifier::ITALIC);
            }
            if style.is_underline {
                out = out.add_modifier(Modifier::UNDERLINED);
            }
            if style.is_reverse {
                out = out.add_modifier(Modifier::REVERSED);
            }
            Span::styled(fragment.clone(), out)
        })
        .collect()
}

fn nu_color_to_ratatui(color: nu_ansi_term::Color) -> Option<Color> {
    use nu_ansi_term::Color as N;
    Some(match color {
        N::Black => Color::Black,
        N::DarkGray => Color::DarkGray,
        N::Red => Color::Red,
        N::LightRed => Color::LightRed,
        N::Green => Color::Green,
        N::LightGreen => Color::LightGreen,
        N::Yellow => Color::Yellow,
        N::LightYellow => Color::LightYellow,
        N::Blue => Color::Blue,
        N::LightBlue => Color::LightBlue,
        N::Purple | N::Magenta => Color::Magenta,
        N::LightPurple | N::LightMagenta => Color::LightMagenta,
        N::Cyan => Color::Cyan,
        N::LightCyan => Color::LightCyan,
        N::White | N::LightGray => Color::White,
        N::Default => return None,
        N::Fixed(n) => Color::Indexed(n),
        N::Rgb(r, g, b) => Color::Rgb(r, g, b),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_editor_cursor_navigation_handles_unicode() {
        let mut editor = LineEditor::new();
        editor.insert('a');
        editor.insert('é');
        editor.insert('b');
        assert_eq!(editor.buffer, "aéb");
        editor.home();
        assert_eq!(editor.cursor, 0);
        editor.move_right(); // past 'a'
        editor.move_right(); // past 'é' (2 bytes)
        assert_eq!(editor.cursor, 3);
        editor.move_left();
        assert_eq!(editor.cursor, 1);
        editor.backspace(); // remove 'a'
        assert_eq!(editor.buffer, "éb");
    }

    #[test]
    fn line_editor_history_walks_up_and_back_down_to_stash() {
        let mut editor = LineEditor::new();
        editor.record("one".into());
        editor.record("two".into());
        editor.buffer.push_str("in-flight");
        editor.cursor = editor.buffer.len();

        editor.history_prev();
        assert_eq!(editor.buffer, "two");
        editor.history_prev();
        assert_eq!(editor.buffer, "one");
        editor.history_next();
        assert_eq!(editor.buffer, "two");
        editor.history_next();
        assert_eq!(editor.buffer, "in-flight");
    }

    #[test]
    fn line_editor_cycles_completions_in_place() {
        let mut editor = LineEditor::new();
        editor.buffer.push_str("ses");
        editor.cursor = editor.buffer.len();
        let ctx = Arc::new(Mutex::new(ReplCtx::new()));
        let mut completer = ReplCompleter::new(ctx, PluginCache::new());
        editor.cycle_completion(&mut completer);
        assert!(
            editor.buffer.starts_with("session."),
            "expected first completion to start with `session.`, got `{}`",
            editor.buffer
        );
        let first = editor.buffer.clone();
        editor.cycle_completion(&mut completer);
        // Cycling should change the buffer (multiple session.* candidates).
        assert_ne!(editor.buffer, first);
    }
}
