//! Mutable application state for the ratatui REPL.
//!
//! The [`App`] struct is the single owner of every piece of state the
//! draw functions or event handlers read: tabs/focus, command log, input
//! buffer, completion popup, history navigation cursor, latest snapshot
//! per adapter, and the dispatched-command back-channels.
//!
//! No claude-code (or other application-specific) identifiers live in
//! this file. All adapter ids, plugin names, intent strings, and state
//! labels are opaque strings carried verbatim through the generic
//! `adapter.*` RPC surface.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::command::{self, Cmd, CmdOutcome};
use super::completer::{AdapterCache, PluginCache, ReplCompleter, Suggestion};
use super::ctx::ReplCtx;
use super::dispatcher::{FocusInfo, UiEvent};
use super::history::ReplHistory;
use super::notes::Note;
use super::transport::RpcClient;
use super::tui::RPC_TIMEOUT;
use crate::screen::ScreenSnapshot;

/// Maximum number of log entries kept in memory. The renderer caps the
/// visible window; older entries scroll off the top of the buffer.
const MAX_LOG_ENTRIES: usize = 500;

/// One entry in the scrolling command log. Variants paint differently in
/// [`super::render::render_log`].
#[derive(Debug, Clone)]
pub enum LogEntry {
    /// `pty> <command>` — the line the operator submitted.
    Input(String),
    /// `↳ <styled note>` — a structured success summary.
    Note(Note),
    /// `↳ <plain text>` — a freeform success line (e.g. `:tabs`).
    Line(String),
    /// JSON pretty-printed response (`:rpc`, `inspect()`, …).
    Json(String),
    /// `✗ <error>` — dispatcher returned an error.
    Error(String),
    /// `[notif] …` — surviving server notification (e.g. `session.exited`).
    Notice(String),
    /// Help text drop — printed verbatim across multiple lines.
    Help(String),
    /// Banner line printed once at startup.
    Banner(String),
    /// Highlighted hint banner (e.g. live adapters discovered at startup).
    Hint(String),
}

/// REPL application state. One per `ptywright repl` invocation.
pub struct App {
    client: Arc<RpcClient>,
    ctx: Arc<Mutex<ReplCtx>>,
    completer: ReplCompleter,
    history: ReplHistory,
    transport_label: String,
    #[allow(dead_code)]
    ui_tx: Sender<UiEvent>,
    focus_tx: Sender<FocusInfo>,
    pub(super) plugins: PluginCache,
    #[allow(dead_code)]
    pub(super) adapters_cache: AdapterCache,

    pub(super) input: Input,
    pub(super) log: VecDeque<LogEntry>,
    pub(super) snapshots: HashMap<String, ScreenSnapshot>,
    pub(super) completions: Option<CompletionState>,
    pub(super) history_pos: Option<usize>,
    pub(super) history_stash: Option<String>,
    pub(super) should_quit: bool,
    #[allow(dead_code)]
    pub(super) status: Option<String>,
    /// Maps adapter id → session id learned from `adapter.start` /
    /// `adapter.live` responses. The dispatcher needs this to filter
    /// `session.changed` notifications: the server emits them keyed by
    /// session id, but the REPL tracks focus by adapter id.
    pub(super) adapter_sessions: HashMap<String, String>,
}

/// State for an open completion popup. `index` is which candidate is
/// currently highlighted; cycling wraps around inside the list.
/// `original_buffer` / `original_cursor` snapshot the input at the
/// moment the popup opened — every applied suggestion replaces its
/// `span` in *that* buffer, not in the buffer modified by the previous
/// suggestion. Without this, cycling past the first candidate corrupts
/// the input (the second suggestion replaces only the original prefix
/// span, leaving the tail of the first candidate behind).
#[derive(Debug, Clone)]
pub(super) struct CompletionState {
    pub suggestions: Vec<Suggestion>,
    pub index: usize,
    pub original_buffer: String,
    #[allow(dead_code)]
    pub original_cursor: usize,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<RpcClient>,
        ctx: Arc<Mutex<ReplCtx>>,
        completer: ReplCompleter,
        history: ReplHistory,
        transport_label: String,
        ui_tx: Sender<UiEvent>,
        focus_tx: Sender<FocusInfo>,
        plugins: PluginCache,
        adapters_cache: AdapterCache,
    ) -> Self {
        let mut app = Self {
            client,
            ctx,
            completer,
            history,
            transport_label,
            ui_tx,
            focus_tx,
            plugins,
            adapters_cache,
            input: Input::default(),
            log: VecDeque::with_capacity(64),
            snapshots: HashMap::new(),
            completions: None,
            history_pos: None,
            history_stash: None,
            should_quit: false,
            status: None,
            adapter_sessions: HashMap::new(),
        };
        app.push_log(LogEntry::Banner(format!(
            "ptywright repl  {}",
            app.transport_label
        )));
        app.push_log(LogEntry::Banner(
            "Type :help for commands · :quit to exit · Tab cycles completions".into(),
        ));
        app
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn transport_label(&self) -> &str {
        &self.transport_label
    }

    pub fn ctx(&self) -> Arc<Mutex<ReplCtx>> {
        Arc::clone(&self.ctx)
    }

    pub fn focused_snapshot(&self) -> Option<(String, &ScreenSnapshot)> {
        let ctx = self.ctx.lock().ok()?;
        let id = ctx.focus.as_ref()?.clone();
        drop(ctx);
        let snap = self.snapshots.get(&id)?;
        Some((id, snap))
    }

    pub fn set_live_hint(&mut self, hint: Option<String>) {
        if let Some(text) = hint {
            self.push_log(LogEntry::Hint(text));
        }
    }

    /// Apply a background event from the dispatcher thread.
    pub fn apply_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Snapshot { adapter, snapshot } => {
                self.snapshots.insert(adapter, snapshot);
            }
            UiEvent::Notice(text) => self.push_log(LogEntry::Notice(text)),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        for ch in text.chars() {
            // Reuse tui-input's char-insert path so cursor + visual width
            // bookkeeping stays consistent.
            self.input.handle(tui_input::InputRequest::InsertChar(ch));
        }
        self.cancel_completion();
        self.cancel_history_nav();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        // Global shortcuts first — these are not routed through tui-input.
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.input.value().is_empty() {
                    self.should_quit = true;
                    return;
                }
                self.input.reset();
                self.cancel_completion();
                self.cancel_history_nav();
                return;
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.input.value().is_empty() => {
                self.should_quit = true;
                return;
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                self.log.clear();
                return;
            }
            (KeyCode::Enter, _) => {
                self.submit();
                return;
            }
            (KeyCode::Esc, _) => {
                if self.completions.is_some() {
                    self.cancel_completion();
                    return;
                }
                self.cancel_history_nav();
                return;
            }
            (KeyCode::Tab, _) => {
                self.advance_completion(1);
                return;
            }
            (KeyCode::BackTab, _) => {
                self.advance_completion(-1);
                return;
            }
            (KeyCode::Up, _) => {
                self.history_nav(1);
                return;
            }
            (KeyCode::Down, _) => {
                self.history_nav(-1);
                return;
            }
            _ => {}
        }

        // Forward everything else to tui-input.
        let event = ratatui::crossterm::event::Event::Key(key);
        let changed = self.input.handle_event(&event).is_some();
        if changed {
            self.cancel_completion();
            self.cancel_history_nav();
        }
    }

    fn submit(&mut self) {
        let line = self.input.value().trim().to_string();
        self.cancel_completion();
        self.cancel_history_nav();
        if line.is_empty() {
            return;
        }
        let _ = self.history.push(&line);
        self.input.reset();
        self.push_log(LogEntry::Input(line.clone()));
        match command::parse(&line) {
            Ok(cmd) => self.dispatch(cmd),
            Err(error) => self.push_log(LogEntry::Error(error.to_string())),
        }
        // Tell the dispatcher about a possible focus change after every
        // command — `session.spawn`, `:focus`, `:attach`, etc. all shift
        // which adapter the snapshot pane should poll. Send the paired
        // (adapter, session) so the dispatcher can match `session.changed`
        // notifications by session id.
        let adapter = self.ctx.lock().ok().and_then(|c| c.focus.clone());
        let session = adapter
            .as_ref()
            .and_then(|id| self.adapter_sessions.get(id).cloned());
        let _ = self.focus_tx.send(FocusInfo { adapter, session });
    }

    fn dispatch(&mut self, cmd: Cmd) {
        let outcome = {
            let mut ctx = self.ctx.lock().expect("repl ctx mutex");
            command::dispatch(cmd, &self.client, &mut ctx, RPC_TIMEOUT)
        };
        match outcome {
            Ok(CmdOutcome::Quit) => self.should_quit = true,
            Ok(CmdOutcome::ShowHelp(text)) => self.push_log(LogEntry::Help(text)),
            Ok(CmdOutcome::Line(text)) => self.push_log(LogEntry::Line(text)),
            Ok(CmdOutcome::Json(value)) => {
                let pretty = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
                self.push_log(LogEntry::Json(truncate(pretty, 280)));
            }
            Ok(CmdOutcome::Note { note, value }) => {
                self.push_log(LogEntry::Note(note));
                // `adapter.start` / `adapter.resume` responses include
                // both fields — stash the mapping so the dispatcher can
                // filter session.changed by session id. The map is also
                // populated lazily when adapters are attached.
                if let (Some(adapter), Some(session)) = (
                    value.get("adapter").and_then(Value::as_str),
                    value.get("session").and_then(Value::as_str),
                ) {
                    self.adapter_sessions
                        .insert(adapter.to_string(), session.to_string());
                }
                // If the response carries a usable snapshot already (e.g.
                // adapter.snapshot), cache it so the live pane updates
                // without waiting for the next session.changed.
                if let Some(snapshot) = value.get("snapshot")
                    && let Ok(snap) = serde_json::from_value::<ScreenSnapshot>(snapshot.clone())
                    && let Some(adapter) = value.get("adapter").and_then(Value::as_str)
                {
                    self.snapshots.insert(adapter.to_string(), snap);
                }
            }
            Ok(CmdOutcome::Screen { adapter, snapshot }) => {
                self.snapshots.insert(adapter.clone(), snapshot);
                self.push_log(LogEntry::Line(format!(
                    "snapshot · {adapter} (rendered in the live pane)"
                )));
            }
            Err(error) => self.push_log(LogEntry::Error(error.to_string())),
        }
    }

    // ---- completion ---------------------------------------------------

    fn advance_completion(&mut self, direction: i32) {
        if let Some(state) = self.completions.as_mut() {
            if state.suggestions.is_empty() {
                self.completions = None;
                return;
            }
            let len = state.suggestions.len() as i32;
            let next = (state.index as i32 + direction).rem_euclid(len);
            state.index = next as usize;
            apply_suggestion(
                &mut self.input,
                &state.original_buffer,
                &state.suggestions[state.index],
            );
            return;
        }
        let buffer = self.input.value().to_string();
        let cursor = self.input.cursor();
        let suggestions = self
            .completer
            .complete(&buffer, byte_offset(&buffer, cursor));
        if suggestions.is_empty() {
            return;
        }
        if suggestions.len() == 1 {
            apply_suggestion(&mut self.input, &buffer, &suggestions[0]);
            self.completions = None;
            return;
        }
        let initial_index = if direction == -1 {
            suggestions.len() - 1
        } else {
            0
        };
        let state = CompletionState {
            suggestions,
            index: initial_index,
            original_buffer: buffer.clone(),
            original_cursor: cursor,
        };
        apply_suggestion(&mut self.input, &buffer, &state.suggestions[state.index]);
        self.completions = Some(state);
    }

    fn cancel_completion(&mut self) {
        self.completions = None;
    }

    // ---- history navigation ------------------------------------------

    fn history_nav(&mut self, delta: i32) {
        if self.history.is_empty() {
            return;
        }
        if self.history_pos.is_none() {
            self.history_stash = Some(self.input.value().to_string());
        }
        let max = self.history.len();
        let current = self.history_pos.map(|p| p as i32).unwrap_or(-1);
        let mut next = current + delta;
        if next < -1 {
            next = -1;
        }
        if next as usize >= max {
            next = max as i32 - 1;
        }
        if next < 0 {
            self.history_pos = None;
            let stash = self.history_stash.take().unwrap_or_default();
            self.input = Input::default().with_value(stash);
        } else {
            self.history_pos = Some(next as usize);
            if let Some(entry) = self.history.nth_back(next as usize) {
                self.input = Input::default().with_value(entry.to_string());
            }
        }
    }

    fn cancel_history_nav(&mut self) {
        self.history_pos = None;
        self.history_stash = None;
    }

    // ---- log housekeeping --------------------------------------------

    pub(super) fn push_log(&mut self, entry: LogEntry) {
        self.log.push_back(entry);
        while self.log.len() > MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
    }
}

fn apply_suggestion(input: &mut Input, current: &str, suggestion: &Suggestion) {
    let span_start = suggestion.span.start.min(current.len());
    let span_end = suggestion.span.end.min(current.len());
    let mut next = String::with_capacity(current.len() + suggestion.value.len());
    next.push_str(&current[..span_start]);
    next.push_str(&suggestion.value);
    next.push_str(&current[span_end..]);
    let cursor = next[..span_start + suggestion.value.len()].chars().count();
    *input = Input::default().with_value(next).with_cursor(cursor);
}

fn byte_offset(s: &str, cursor_codepoints: usize) -> usize {
    s.char_indices()
        .nth(cursor_codepoints)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        s = s.chars().take(max).collect::<String>();
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::Framing;
    use crate::repl::completer::AdapterCache;

    fn make_app() -> App {
        let (read, write) = pipe::pipe();
        let client = RpcClient::new(read, write, Framing::Ndjson);
        let ctx = Arc::new(Mutex::new(ReplCtx::new()));
        let plugins = PluginCache::new();
        let adapters = AdapterCache::new();
        let completer = ReplCompleter::new(Arc::clone(&ctx), plugins.clone(), adapters.clone());
        let path = std::env::temp_dir().join(format!(
            "ptywright-app-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let history = ReplHistory::open(&path).expect("open history");
        let (ui_tx, _ui_rx) = crossbeam_channel::unbounded();
        let (focus_tx, _focus_rx) = crossbeam_channel::unbounded();
        App::new(
            client,
            ctx,
            completer,
            history,
            "socket:/tmp/test".into(),
            ui_tx,
            focus_tx,
            plugins,
            adapters,
        )
    }

    #[test]
    fn truncate_respects_max_codepoints() {
        assert_eq!(truncate("hello".to_string(), 10), "hello");
        let result = truncate("é".repeat(10), 5);
        assert_eq!(result.chars().count(), 6); // 5 chars + ellipsis
        assert!(result.ends_with('…'));
    }

    #[test]
    fn byte_offset_handles_multibyte() {
        let s = "héllo";
        assert_eq!(byte_offset(s, 0), 0);
        assert_eq!(byte_offset(s, 1), 1);
        assert_eq!(byte_offset(s, 2), 3); // é is two bytes
        assert_eq!(byte_offset(s, 5), s.len());
    }

    #[test]
    fn apply_suggestion_replaces_span() {
        let mut input = Input::default()
            .with_value(":focus e".to_string())
            .with_cursor(8);
        let suggestion = Suggestion {
            value: "e1".into(),
            description: None,
            span: super::super::completer::Span { start: 7, end: 8 },
        };
        apply_suggestion(&mut input, ":focus e", &suggestion);
        assert_eq!(input.value(), ":focus e1");
    }

    #[test]
    fn app_starts_with_banner_lines() {
        let app = make_app();
        assert!(matches!(app.log.front(), Some(LogEntry::Banner(_))));
        assert!(app.log.len() >= 2);
    }

    #[test]
    fn tab_with_unique_match_inserts_completion() {
        let mut app = make_app();
        app.input = Input::default()
            .with_value("ses".to_string())
            .with_cursor(3);
        app.advance_completion(1);
        // The DSL table has multiple `session.*` entries — tab opens the
        // popup but already inserts the first candidate. Cycling Tab again
        // should advance to the second.
        let first = app.input.value().to_string();
        app.advance_completion(1);
        let second = app.input.value().to_string();
        assert!(first.starts_with("session."));
        assert!(second.starts_with("session."));
        assert_ne!(first, second);
    }

    #[test]
    fn cycling_completion_replaces_against_the_original_buffer() {
        // Regression: pressing Tab twice from a 3-char prefix used to
        // replace only the first 3 bytes of the *already-expanded* buffer,
        // leaving the tail of the first candidate stuck on the end of the
        // second one. Confirm cycling now always produces a clean
        // candidate that *starts with* the original prefix and *equals* a
        // known DSL form.
        let mut app = make_app();
        app.input = Input::default()
            .with_value("ses".to_string())
            .with_cursor(3);
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..6 {
            app.advance_completion(1);
            seen.push(app.input.value().to_string());
        }
        for value in &seen {
            assert!(value.starts_with("session."), "got: {value}");
            // None of the buffers should contain an interior `(` followed
            // by `session.` — that's the corruption signature.
            assert!(
                !value
                    .split_once("(")
                    .map_or(false, |(_, after)| after.contains("session.")),
                "buffer corruption: {value}"
            );
        }
    }

    #[test]
    fn esc_clears_completion_state() {
        let mut app = make_app();
        app.input = Input::default()
            .with_value("ses".to_string())
            .with_cursor(3);
        app.advance_completion(1);
        assert!(app.completions.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.completions.is_none());
    }

    #[test]
    fn ctrl_c_clears_input_first_then_quits() {
        let mut app = make_app();
        app.input = Input::default().with_value("session.spawn".to_string());
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value(), "");
        assert!(!app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn up_arrow_walks_history_back() {
        let mut app = make_app();
        let _ = app.history.push("alpha");
        let _ = app.history.push("beta");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "beta");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "alpha");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "beta");
    }

    mod pipe {
        use std::io::{Read, Result, Write};
        use std::sync::mpsc::{Receiver, Sender, channel};

        pub fn pipe() -> (PipeReader, PipeWriter) {
            let (tx, rx) = channel::<Vec<u8>>();
            (
                PipeReader {
                    rx,
                    buf: Vec::new(),
                },
                PipeWriter { tx },
            )
        }

        pub struct PipeReader {
            rx: Receiver<Vec<u8>>,
            buf: Vec<u8>,
        }
        pub struct PipeWriter {
            tx: Sender<Vec<u8>>,
        }

        impl Read for PipeReader {
            fn read(&mut self, out: &mut [u8]) -> Result<usize> {
                while self.buf.is_empty() {
                    match self.rx.recv() {
                        Ok(chunk) => self.buf.extend_from_slice(&chunk),
                        Err(_) => return Ok(0),
                    }
                }
                let n = out.len().min(self.buf.len());
                out[..n].copy_from_slice(&self.buf[..n]);
                self.buf.drain(..n);
                Ok(n)
            }
        }
        impl Write for PipeWriter {
            fn write(&mut self, buf: &[u8]) -> Result<usize> {
                self.tx.send(buf.to_vec()).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string())
                })?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> Result<()> {
                Ok(())
            }
        }
    }
}
