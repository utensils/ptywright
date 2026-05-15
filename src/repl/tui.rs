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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
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

const EVENT_POLL: Duration = Duration::from_millis(50);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const HISTORY_CAPACITY: usize = 200;

/// Public entry: build state and run the event loop until the user quits.
pub fn run(client: Arc<RpcClient>, transport_label: String) -> Result<()> {
    let ctx = Arc::new(Mutex::new(ReplCtx::new()));
    let store = Arc::new(SnapshotStore::new());
    let (redraw_tx, redraw_rx) = bounded::<()>(1);

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
        },
    );

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
}

fn event_loop(terminal: &mut DefaultTerminal, mut args: EventLoopArgs<'_>) -> Result<()> {
    let mut editor = LineEditor::new();
    loop {
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &args.transport_label,
                    &args.ctx,
                    &args.store,
                    &editor,
                    &args.highlighter,
                    *args.notifications_enabled,
                );
            })
            .map_err(|error| Error::Rpc(format!("draw frame: {error}")))?;

        let event = poll_event(&args.redraw_rx)?;
        if let Some(event) = event {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                Event::Resize(_, _) | Event::Mouse(MouseEvent { .. }) => { /* redraw covers it */ }
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
        }
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
            // First Tab: query the completer and stash the origin span +
            // base text so subsequent Tabs cycle through alternatives.
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
        self.replace_span(span, &pick);
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

fn render(
    frame: &mut ratatui::Frame<'_>,
    transport_label: &str,
    ctx: &Arc<Mutex<ReplCtx>>,
    store: &Arc<SnapshotStore>,
    editor: &LineEditor,
    highlighter: &ReplHighlighter,
    notifications_enabled: bool,
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
    let tab_line = ctx_snapshot
        .as_ref()
        .map(render_tabs)
        .unwrap_or_else(|| Line::from(""));
    frame.render_widget(Paragraph::new(tab_line), chunks[1]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(chunks[2]);

    render_preview_pane(frame, main[0], ctx_snapshot.as_ref(), store);
    render_history_pane(frame, main[1], ctx_snapshot.as_ref());

    render_input(frame, chunks[3], editor, highlighter);

    let footer_text = footer_line(ctx_snapshot.as_ref(), editor, notifications_enabled);
    frame.render_widget(Paragraph::new(footer_text), chunks[4]);
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

fn footer_line(
    ctx: Option<&ReplCtx>,
    editor: &LineEditor,
    notifications_enabled: bool,
) -> Line<'static> {
    let focus = ctx
        .and_then(|c| c.focus.clone())
        .unwrap_or_else(|| "—".to_string());
    let cmd_count = ctx.map(|c| c.history.len()).unwrap_or_default();
    let notif = if notifications_enabled {
        "notif on"
    } else {
        "notif off"
    };
    let history_len = editor.history.len();
    let dim = Style::default().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::styled(focus, Style::default().fg(Color::Cyan)),
        Span::styled("  ·  ", dim),
        Span::styled(notif.to_string(), dim),
        Span::styled("  ·  ", dim),
        Span::styled(format!("{cmd_count} entries"), dim),
        Span::styled("  ·  ", dim),
        Span::styled(format!("{history_len} recalled"), dim),
        Span::styled("  ·  Alt-1..9 focus · Ctrl-C clear · Ctrl-D quit", dim),
    ])
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
