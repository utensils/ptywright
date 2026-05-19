//! REPL command DSL — lexer, parser, and JSON-RPC dispatcher.
//!
//! The DSL is intentionally tiny: dotted-path method calls (`session.spawn`,
//! `send.text`, `wait.screen_stable`) with positional + keyword args, plus a
//! handful of `:meta` commands for REPL-local actions and a `:rpc` escape
//! hatch for raw JSON-RPC. Everything translates to `adapter.*` calls on
//! the generic surface — there is no per-plugin Rust code here, only string
//! literals that flow through to plugin intents.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::ctx::ReplCtx;
use super::transport::RpcClient;
use crate::error::{Error, Result};

/// Top-level command after parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Friendly DSL form: `path(args...)`.
    Dsl(DslCall),
    /// `:meta` REPL-local actions (`:tabs`, `:quit`, etc.) and the `:rpc`
    /// raw-call escape hatch.
    Meta(MetaCmd),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DslCall {
    pub path: Vec<String>,
    pub positional: Vec<Arg>,
    pub kwargs: BTreeMap<String, Arg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    String(String),
    Regex(String),
    Duration(Duration),
    Int(i64),
    Bool(bool),
    Null,
    List(Vec<Arg>),
    Object(BTreeMap<String, Arg>),
    Call(DslCall),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetaCmd {
    Help,
    Quit,
    Tabs,
    Focus(String),
    Notifications(NotificationsRequest),
    /// List adapters live on the server (across all connections).
    Live,
    /// Attach a server-side adapter into this REPL's local tab list.
    /// `AttachSpec::All` pulls everything live; `AttachSpec::One` attaches
    /// a single adapter id and auto-renders its current screen.
    Attach(AttachSpec),
    Rpc {
        method: String,
        params: Value,
    },
}

/// What to attach. Mirrors the `:attach` arg surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachSpec {
    One(String),
    All,
}

/// Per-stream notification filter, optionally scoping the event stream
/// to a named adapter / session list. Empty vectors map to "no filter
/// in force" — the server delivers every session. Mirrors the
/// `server.set_notifications` `adapters` / `sessions` arrays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationsRequest {
    pub enabled: bool,
    pub adapters: Vec<String>,
    pub sessions: Vec<String>,
}

impl NotificationsRequest {
    pub fn simple(enabled: bool) -> Self {
        Self {
            enabled,
            adapters: Vec::new(),
            sessions: Vec::new(),
        }
    }
}

/// Outcome of a dispatched command — what the TUI's history pane renders.
#[derive(Debug, Clone)]
pub enum CmdOutcome {
    /// Human-readable line (the dispatched command produced no result we
    /// want to dump into the history pane, e.g. `:tabs` or `:focus`).
    Line(String),
    /// JSON result from a JSON-RPC call. The TUI pretty-prints it. Used
    /// when the operator probably wants the raw payload (`:rpc`,
    /// `:live`, `plugins.describe`, `state`, `inspect`).
    Json(Value),
    /// A structured note for the `↳` summary line plus the underlying
    /// RPC value. The TUI surfaces the note's [`render`](crate::repl::notes::Note::render)
    /// output; tests and `:debug` modes can still inspect `value`.
    /// Use this for commands where a pretty summary is more useful than
    /// the raw JSON: `session.spawn`, `session.close`, `send.text`,
    /// `send.key`, `send.intent`, `wait`, `turn`, `transcript.snapshot`.
    Note {
        note: crate::repl::notes::Note,
        value: Value,
    },
    /// Multi-line help text the TUI should surface as a modal overlay
    /// rather than squeezing into the history pane.
    ShowHelp(String),
    /// Rendered terminal snapshot. The TUI prints it inline with cell
    /// styles so the operator can see the PTY exactly as the agent does.
    Screen {
        adapter: String,
        snapshot: crate::screen::ScreenSnapshot,
    },
    /// REPL should exit.
    Quit,
}

// ---- lexer ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    String(String),
    Regex(String),
    Int(i64),
    Duration(Duration),
    Dot,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Eq,
    Colon,
}

#[derive(Debug)]
struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self) {
        if let Some(ch) = self.peek() {
            self.pos += ch.len_utf8();
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn lex_number_or_duration(&mut self) -> Result<Tok> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.advance();
        }
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        let digits = &self.input[start..self.pos];
        // Optional duration suffix.
        let suffix_start = self.pos;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphabetic() {
                self.advance();
            } else {
                break;
            }
        }
        let suffix = &self.input[suffix_start..self.pos];
        if suffix.is_empty() {
            let value: i64 = digits
                .parse()
                .map_err(|error| Error::Rpc(format!("invalid integer `{digits}`: {error}")))?;
            return Ok(Tok::Int(value));
        }
        let value: u64 = digits
            .parse()
            .map_err(|error| Error::Rpc(format!("invalid duration `{digits}{suffix}`: {error}")))?;
        let duration = match suffix {
            "ms" => Duration::from_millis(value),
            "s" => Duration::from_secs(value),
            other => {
                return Err(Error::Rpc(format!(
                    "unknown duration unit `{other}` — expected ms or s"
                )));
            }
        };
        Ok(Tok::Duration(duration))
    }

    fn lex_ident(&mut self) -> Tok {
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }
        Tok::Ident(self.input[start..self.pos].to_string())
    }

    fn lex_string(&mut self) -> Result<String> {
        self.advance(); // consume opening "
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            match ch {
                '"' => {
                    self.advance();
                    return Ok(out);
                }
                '\\' => {
                    self.advance();
                    let escaped = self
                        .peek()
                        .ok_or_else(|| Error::Rpc("unterminated escape in string".to_string()))?;
                    let resolved = match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '"' => '"',
                        '0' => '\0',
                        other => {
                            return Err(Error::Rpc(format!("unknown string escape `\\{other}`")));
                        }
                    };
                    out.push(resolved);
                    self.advance();
                }
                _ => {
                    out.push(ch);
                    self.advance();
                }
            }
        }
        Err(Error::Rpc("unterminated string literal".to_string()))
    }

    fn lex_raw_string(&mut self) -> Result<String> {
        // Caller has consumed `r`; we sit on the opening quote.
        if self.peek() != Some('"') {
            return Err(Error::Rpc("raw string must start with r\"…\"".to_string()));
        }
        self.advance();
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch == '"' {
                let content = self.input[start..self.pos].to_string();
                self.advance();
                return Ok(content);
            }
            self.advance();
        }
        Err(Error::Rpc("unterminated raw string literal".to_string()))
    }

    fn next_token(&mut self) -> Result<Option<Tok>> {
        self.skip_whitespace();
        let ch = match self.peek() {
            Some(c) => c,
            None => return Ok(None),
        };
        let tok = match ch {
            '.' => {
                self.advance();
                Tok::Dot
            }
            '(' => {
                self.advance();
                Tok::LParen
            }
            ')' => {
                self.advance();
                Tok::RParen
            }
            '[' => {
                self.advance();
                Tok::LBracket
            }
            ']' => {
                self.advance();
                Tok::RBracket
            }
            '{' => {
                self.advance();
                Tok::LBrace
            }
            '}' => {
                self.advance();
                Tok::RBrace
            }
            ',' => {
                self.advance();
                Tok::Comma
            }
            '=' => {
                self.advance();
                Tok::Eq
            }
            ':' => {
                self.advance();
                Tok::Colon
            }
            '"' => Tok::String(self.lex_string()?),
            'r' if self.input[self.pos + 1..].starts_with('"') => {
                self.advance();
                Tok::Regex(self.lex_raw_string()?)
            }
            c if c.is_ascii_digit() || c == '-' => self.lex_number_or_duration()?,
            c if c.is_ascii_alphabetic() || c == '_' => self.lex_ident(),
            other => {
                return Err(Error::Rpc(format!("unexpected character `{other}`")));
            }
        };
        Ok(Some(tok))
    }

    fn tokens(mut self) -> Result<Vec<Tok>> {
        let mut out = Vec::new();
        while let Some(tok) = self.next_token()? {
            out.push(tok);
        }
        Ok(out)
    }
}

// ---- parser -----------------------------------------------------------

/// Parse a complete input line into a [`Cmd`]. Empty/whitespace-only input
/// returns [`Error::Rpc`] so the dispatcher can choose to no-op.
pub fn parse(input: &str) -> Result<Cmd> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::Rpc("empty command".to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix(':') {
        return parse_meta(rest);
    }
    let tokens = Lexer::new(trimmed).tokens()?;
    let mut parser = Parser::new(tokens);
    let call = parser.parse_dotted_call()?;
    parser.expect_end()?;
    Ok(Cmd::Dsl(call))
}

fn parse_meta(rest: &str) -> Result<Cmd> {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let tail = parts.next().unwrap_or("").trim();
    let meta = match head {
        "help" => MetaCmd::Help,
        "quit" | "q" | "exit" => MetaCmd::Quit,
        "tabs" => MetaCmd::Tabs,
        "focus" => {
            if tail.is_empty() {
                return Err(Error::Rpc(":focus requires an adapter id".to_string()));
            }
            MetaCmd::Focus(tail.to_string())
        }
        "notifications" => MetaCmd::Notifications(parse_notifications_tail(tail)?),
        "live" => MetaCmd::Live,
        "attach" => {
            if tail.is_empty() {
                return Err(Error::Rpc(
                    ":attach requires an adapter id or `all`".to_string(),
                ));
            }
            let spec = if tail == "all" {
                AttachSpec::All
            } else {
                AttachSpec::One(tail.to_string())
            };
            MetaCmd::Attach(spec)
        }
        "rpc" => {
            let mut split = tail.splitn(2, char::is_whitespace);
            let method = split
                .next()
                .filter(|m| !m.is_empty())
                .ok_or_else(|| Error::Rpc(":rpc requires a method name".to_string()))?
                .to_string();
            let params_text = split.next().unwrap_or("{}").trim();
            let params: Value = if params_text.is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(params_text).map_err(|error| {
                    Error::Rpc(format!(":rpc params are not valid JSON: {error}"))
                })?
            };
            MetaCmd::Rpc { method, params }
        }
        other => {
            return Err(Error::Rpc(format!("unknown meta command `:{other}`")));
        }
    };
    Ok(Cmd::Meta(meta))
}

/// Parse the trailing args of `:notifications` into a [`NotificationsRequest`].
///
/// Supported forms:
///
/// ```text
/// :notifications on
/// :notifications off
/// :notifications on adapters=e1,e2
/// :notifications on sessions=s1,s2
/// :notifications on adapters=e1 sessions=s1
/// ```
///
/// `on`/`off` may be written as `true`/`false`/`1`/`0` for symmetry with the
/// boolean kwargs accepted elsewhere. `adapters=` and `sessions=` take a
/// comma-separated id list. Whitespace is the only separator between the
/// boolean head and the filter kwargs.
fn parse_notifications_tail(tail: &str) -> Result<NotificationsRequest> {
    let mut parts = tail.split_whitespace();
    let head = parts.next().unwrap_or("");
    let enabled = match head {
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            return Err(Error::Rpc(format!(
                ":notifications expects on|off [adapters=…] [sessions=…], got `{other}`",
            )));
        }
    };
    let mut adapters: Vec<String> = Vec::new();
    let mut sessions: Vec<String> = Vec::new();
    // `current` tracks which filter list a bare continuation token
    // belongs to. This lets `:notifications on adapters=e1, e2` (with
    // a space after the comma) parse as `adapters=[e1, e2]` instead of
    // erroring on `e2` as an unknown token — the comma+space form is
    // what operators reach for naturally and matches how the same
    // filter would be written in plain English.
    let mut current: Option<&mut Vec<String>> = None;
    for part in parts {
        if let Some(rest) = part.strip_prefix("adapters=") {
            adapters.extend(parse_id_list(rest));
            current = Some(&mut adapters);
        } else if let Some(rest) = part.strip_prefix("sessions=") {
            sessions.extend(parse_id_list(rest));
            current = Some(&mut sessions);
        } else if let Some(target) = current.as_deref_mut() {
            // Continuation: the previous token left a trailing comma
            // or stopped on whitespace inside a list, so this is more
            // ids for the same filter list.
            target.extend(parse_id_list(part));
        } else {
            return Err(Error::Rpc(format!(
                ":notifications: unknown filter token `{part}` (expected `adapters=…` or `sessions=…`)",
            )));
        }
    }
    if !enabled && (!adapters.is_empty() || !sessions.is_empty()) {
        return Err(Error::Rpc(
            ":notifications off does not take filter args".to_string(),
        ));
    }
    Ok(NotificationsRequest {
        enabled,
        adapters,
        sessions,
    })
}

fn parse_id_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Tok>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        tok
    }

    fn expect_end(&self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(tok) => Err(Error::Rpc(format!("unexpected trailing token {tok:?}"))),
        }
    }

    fn parse_dotted_call(&mut self) -> Result<DslCall> {
        let mut path = Vec::new();
        loop {
            match self.bump() {
                Some(Tok::Ident(name)) => path.push(name),
                Some(tok) => {
                    return Err(Error::Rpc(format!("expected identifier, found {tok:?}",)));
                }
                None => return Err(Error::Rpc("expected identifier".to_string())),
            }
            if let Some(Tok::Dot) = self.peek() {
                self.bump();
                continue;
            }
            break;
        }
        let (positional, kwargs) = if let Some(Tok::LParen) = self.peek() {
            self.bump();
            self.parse_arg_list()?
        } else {
            (Vec::new(), BTreeMap::new())
        };
        Ok(DslCall {
            path,
            positional,
            kwargs,
        })
    }

    fn parse_arg_list(&mut self) -> Result<(Vec<Arg>, BTreeMap<String, Arg>)> {
        let mut positional = Vec::new();
        let mut kwargs = BTreeMap::new();
        if let Some(Tok::RParen) = self.peek() {
            self.bump();
            return Ok((positional, kwargs));
        }
        loop {
            // Look ahead for `ident =` to detect kwargs.
            let saved = self.pos;
            if let Some(Tok::Ident(name)) = self.peek().cloned() {
                self.bump();
                if let Some(Tok::Eq) = self.peek() {
                    self.bump();
                    let value = self.parse_arg()?;
                    kwargs.insert(name, value);
                } else {
                    // Not a kwarg — rewind and parse as a positional call.
                    self.pos = saved;
                    let value = self.parse_arg()?;
                    positional.push(value);
                }
            } else {
                let value = self.parse_arg()?;
                positional.push(value);
            }
            match self.bump() {
                Some(Tok::Comma) => continue,
                Some(Tok::RParen) => return Ok((positional, kwargs)),
                Some(tok) => {
                    return Err(Error::Rpc(format!("expected `,` or `)`, found {tok:?}",)));
                }
                None => {
                    return Err(Error::Rpc(
                        "expected `)` to close argument list".to_string(),
                    ));
                }
            }
        }
    }

    fn parse_arg(&mut self) -> Result<Arg> {
        match self.peek().cloned() {
            Some(Tok::String(value)) => {
                self.bump();
                Ok(Arg::String(value))
            }
            Some(Tok::Regex(value)) => {
                self.bump();
                Ok(Arg::Regex(value))
            }
            Some(Tok::Int(value)) => {
                self.bump();
                Ok(Arg::Int(value))
            }
            Some(Tok::Duration(value)) => {
                self.bump();
                Ok(Arg::Duration(value))
            }
            Some(Tok::Ident(name)) => {
                match name.as_str() {
                    "true" => {
                        self.bump();
                        Ok(Arg::Bool(true))
                    }
                    "false" => {
                        self.bump();
                        Ok(Arg::Bool(false))
                    }
                    "null" | "nil" => {
                        self.bump();
                        Ok(Arg::Null)
                    }
                    _ => {
                        // Could be a nested call: `wait(matches(r"..."))`.
                        let call = self.parse_dotted_call()?;
                        Ok(Arg::Call(call))
                    }
                }
            }
            Some(Tok::LBracket) => {
                self.bump();
                Ok(Arg::List(self.parse_list()?))
            }
            Some(Tok::LBrace) => {
                self.bump();
                Ok(Arg::Object(self.parse_object()?))
            }
            Some(tok) => Err(Error::Rpc(format!("expected argument, found {tok:?}"))),
            None => Err(Error::Rpc("expected argument".to_string())),
        }
    }

    fn parse_list(&mut self) -> Result<Vec<Arg>> {
        let mut values = Vec::new();
        if let Some(Tok::RBracket) = self.peek() {
            self.bump();
            return Ok(values);
        }

        loop {
            values.push(self.parse_arg()?);
            match self.bump() {
                Some(Tok::Comma) => continue,
                Some(Tok::RBracket) => return Ok(values),
                Some(tok) => {
                    return Err(Error::Rpc(format!("expected `,` or `]`, found {tok:?}")));
                }
                None => return Err(Error::Rpc("expected `]` to close list".to_string())),
            }
        }
    }

    fn parse_object(&mut self) -> Result<BTreeMap<String, Arg>> {
        let mut values = BTreeMap::new();
        if let Some(Tok::RBrace) = self.peek() {
            self.bump();
            return Ok(values);
        }

        loop {
            let key = match self.bump() {
                Some(Tok::Ident(key)) | Some(Tok::String(key)) => key,
                Some(tok) => {
                    return Err(Error::Rpc(format!("expected object key, found {tok:?}")));
                }
                None => return Err(Error::Rpc("expected object key".to_string())),
            };
            match self.bump() {
                Some(Tok::Colon) | Some(Tok::Eq) => {}
                Some(tok) => {
                    return Err(Error::Rpc(format!(
                        "expected `:` or `=` after object key, found {tok:?}"
                    )));
                }
                None => {
                    return Err(Error::Rpc(
                        "expected `:` or `=` after object key".to_string(),
                    ));
                }
            }
            values.insert(key, self.parse_arg()?);
            match self.bump() {
                Some(Tok::Comma) => continue,
                Some(Tok::RBrace) => return Ok(values),
                Some(tok) => {
                    return Err(Error::Rpc(format!("expected `,` or `}}`, found {tok:?}")));
                }
                None => return Err(Error::Rpc("expected `}` to close object".to_string())),
            }
        }
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::String(s) => write!(f, "{s:?}"),
            Arg::Regex(s) => write!(f, "r{s:?}"),
            Arg::Duration(d) => write!(f, "{}ms", d.as_millis()),
            Arg::Int(n) => write!(f, "{n}"),
            Arg::Bool(b) => write!(f, "{b}"),
            Arg::Null => write!(f, "null"),
            Arg::List(values) => write!(f, "[{} item(s)]", values.len()),
            Arg::Object(values) => write!(f, "{{{} key(s)}}", values.len()),
            Arg::Call(call) => write!(f, "{}(...)", call.path.join(".")),
        }
    }
}

// ---- dispatcher --------------------------------------------------------

/// Borrow-only helper used by the dispatcher to demand a focused adapter
/// and report a friendly error when there is none.
fn focus_or_err(ctx: &ReplCtx) -> Result<&str> {
    ctx.focus.as_deref().ok_or_else(|| {
        Error::Rpc("no focused adapter — spawn one with `session.spawn(...)` first".to_string())
    })
}

/// Dispatch a parsed [`Cmd`] against the JSON-RPC client and mutate the
/// shared REPL context. Returns a [`CmdOutcome`] the TUI surfaces.
pub fn dispatch(
    cmd: Cmd,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    match cmd {
        Cmd::Meta(meta) => dispatch_meta(meta, client, ctx, timeout),
        Cmd::Dsl(call) => dispatch_dsl(call, client, ctx, timeout),
    }
}

fn dispatch_meta(
    meta: MetaCmd,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    match meta {
        MetaCmd::Help => Ok(CmdOutcome::ShowHelp(help_text().to_string())),
        MetaCmd::Quit => Ok(CmdOutcome::Quit),
        MetaCmd::Tabs => {
            let line = if ctx.adapters.is_empty() {
                "(no adapters)".to_string()
            } else {
                ctx.adapters
                    .iter()
                    .map(|tab| {
                        let mark = if ctx.focus.as_deref() == Some(tab.id.as_str()) {
                            "*"
                        } else {
                            ""
                        };
                        format!("{mark}{}:{}", tab.id, tab.plugin)
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            Ok(CmdOutcome::Line(line))
        }
        MetaCmd::Focus(id) => {
            if ctx.adapter(&id).is_none() {
                return Err(Error::Rpc(format!("unknown adapter `{id}`")));
            }
            ctx.focus = Some(id.clone());
            Ok(CmdOutcome::Line(format!("focus → {id}")))
        }
        MetaCmd::Notifications(request) => {
            let mut params = Map::new();
            params.insert("enabled".to_string(), Value::Bool(request.enabled));
            if !request.adapters.is_empty() {
                params.insert(
                    "adapters".to_string(),
                    Value::Array(
                        request
                            .adapters
                            .iter()
                            .map(|id| Value::String(id.clone()))
                            .collect(),
                    ),
                );
            }
            if !request.sessions.is_empty() {
                params.insert(
                    "sessions".to_string(),
                    Value::Array(
                        request
                            .sessions
                            .iter()
                            .map(|id| Value::String(id.clone()))
                            .collect(),
                    ),
                );
            }
            let result = client.call("server.set_notifications", Value::Object(params), timeout)?;
            // Pause the client's heartbeat in lockstep — otherwise it
            // would re-assert `set_notifications {enabled: true}` every
            // interval and silently undo `:notifications off`. The
            // filter contents are passive: the heartbeat only needs the
            // enabled flag to decide whether to re-subscribe.
            client.set_heartbeat_enabled(request.enabled);
            Ok(CmdOutcome::Json(result))
        }
        MetaCmd::Live => session_live(client, timeout),
        MetaCmd::Attach(spec) => session_attach(spec, client, ctx, timeout),
        MetaCmd::Rpc { method, params } => {
            let result = client.call(&method, params, timeout)?;
            Ok(CmdOutcome::Json(result))
        }
    }
}

fn dispatch_dsl(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    match call.path.join(".").as_str() {
        "plugins" => {
            let result = client.call("adapter.list", json!({}), timeout)?;
            Ok(CmdOutcome::Json(result))
        }
        "plugins.describe" => {
            let plugin = expect_one_string(&call, "plugins.describe")?;
            let result = client.call("plugin.describe", json!({ "plugin": plugin }), timeout)?;
            Ok(CmdOutcome::Json(result))
        }
        "session.spawn" => session_spawn(call, client, ctx, timeout),
        "session.resume" => session_resume(call, client, ctx, timeout),
        "session.live" => session_live(client, timeout),
        "session.attach" => {
            let spec = match call.positional.first() {
                None => AttachSpec::All,
                Some(Arg::String(s)) if s == "all" => AttachSpec::All,
                Some(Arg::String(id)) => AttachSpec::One(id.clone()),
                Some(other) => {
                    return Err(Error::Rpc(format!(
                        "session.attach expects an adapter id or \"all\", got {other}"
                    )));
                }
            };
            session_attach(spec, client, ctx, timeout)
        }
        "session.list" => {
            let line = if ctx.adapters.is_empty() {
                "(no adapters)".to_string()
            } else {
                ctx.adapters
                    .iter()
                    .map(|tab| format!("{} ({})", tab.id, tab.plugin))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Ok(CmdOutcome::Line(line))
        }
        "session.close" => session_close(call, client, ctx, timeout),
        "state" => {
            let adapter = focus_or_err(ctx)?.to_string();
            let result = client.call("adapter.state", json!({ "adapter": adapter }), timeout)?;
            Ok(CmdOutcome::Json(result))
        }
        "send.intent" => send_intent(call, client, ctx, timeout),
        "send.text" => {
            let text = expect_one_string(&call, "send.text")?;
            let (intent, params) = send_text_payload(text.clone());
            let value = invoke_intent(client, ctx, intent, params, timeout)?;
            Ok(CmdOutcome::Note {
                note: crate::repl::notes::send_text(&text, &value),
                value,
            })
        }
        "send.key" => {
            let key = expect_one_string(&call, "send.key")?;
            let (intent, params) = send_key_payload(key.clone());
            let value = invoke_intent(client, ctx, intent, params, timeout)?;
            Ok(CmdOutcome::Note {
                note: crate::repl::notes::send_key(&key, &value),
                value,
            })
        }
        "turn" => turn_command(call, client, ctx, timeout),
        "wait" => wait_command(call, client, ctx, timeout),
        "cancel_wait" => cancel_wait_command(call, client, timeout),
        "wait.matches" => {
            let pattern = expect_one_regex(&call, "wait.matches")?;
            wait_with_params(
                client,
                ctx,
                "wait_turn_matcher",
                json!({ "pattern": pattern }),
                duration_kwarg(&call, "timeout")?,
                timeout,
            )
        }
        "wait.screen_stable" => {
            let stable = expect_one_duration(&call, "wait.screen_stable")?;
            wait_with_params(
                client,
                ctx,
                "wait_turn_matcher",
                json!({ "stable_ms": stable.as_millis() }),
                duration_kwarg(&call, "timeout")?,
                timeout,
            )
        }
        "transcript.snapshot" => {
            let adapter = focus_or_err(ctx)?.to_string();
            let redact = bool_kwarg(&call, "redact").unwrap_or(true);
            let result = client.call(
                "adapter.transcript",
                json!({ "adapter": adapter, "redact": redact }),
                timeout,
            )?;
            Ok(CmdOutcome::Note {
                note: crate::repl::notes::transcript(&result),
                value: result,
            })
        }
        "screen.snapshot" | "screen.view" | "view" => {
            let adapter = focus_or_err(ctx)?.to_string();
            // adapter.snapshot is asked to *not* redact so the REPL view
            // matches what the underlying terminal really shows. Operators
            // who need a redacted copy can fall through to
            // `:rpc adapter.snapshot {"adapter":"e1","redact":true}`.
            let result = client.call(
                "adapter.snapshot",
                json!({ "adapter": adapter, "redact": false }),
                timeout,
            )?;
            let snapshot: crate::screen::ScreenSnapshot = serde_json::from_value(result)
                .map_err(|error| Error::Rpc(format!("decode adapter.snapshot: {error}")))?;
            Ok(CmdOutcome::Screen { adapter, snapshot })
        }
        "inspect" => {
            let adapter = focus_or_err(ctx)?.to_string();
            let result = client.call("adapter.inspect", json!({ "adapter": adapter }), timeout)?;
            Ok(CmdOutcome::Json(result))
        }
        other => Err(Error::Rpc(format!(
            "unknown command `{other}` — try `:help`"
        ))),
    }
}

fn session_spawn(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let plugin = expect_one_string(&call, "session.spawn")?;
    let mut params = Map::new();
    params.insert("plugin".to_string(), Value::String(plugin.clone()));
    insert_adapter_start_kwargs(&mut params, &call)?;
    let result = client.call("adapter.start", Value::Object(params), timeout)?;
    adopt_started_adapter(ctx, &plugin, &result);
    Ok(CmdOutcome::Note {
        note: crate::repl::notes::spawned(&result),
        value: result,
    })
}

fn session_resume(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let plugin = expect_one_string(&call, "session.resume")?;
    let mut params = Map::new();
    params.insert("plugin".to_string(), Value::String(plugin.clone()));
    insert_adapter_start_kwargs(&mut params, &call)?;
    let prior_adapter = prior_adapter_kwarg(&call)?;
    if let Some(prior) = prior_adapter.as_deref() {
        params.insert(
            "prior_adapter".to_string(),
            Value::String(prior.to_string()),
        );
    }
    let result = client.call("adapter.resume", Value::Object(params), timeout)?;
    if let Some(prior) = prior_adapter.as_deref() {
        ctx.remove_adapter(prior);
    }
    adopt_started_adapter(ctx, &plugin, &result);
    Ok(CmdOutcome::Note {
        note: crate::repl::notes::spawned(&result),
        value: result,
    })
}

fn adopt_started_adapter(ctx: &mut ReplCtx, requested_plugin: &str, result: &Value) {
    if let Some(adapter) = result.get("adapter").and_then(Value::as_str) {
        let response_plugin = result
            .get("plugin")
            .and_then(Value::as_str)
            .unwrap_or(requested_plugin);
        ctx.upsert_adapter(adapter, response_plugin);
        ctx.focus = Some(adapter.to_string());
    }
}

fn session_live(client: &RpcClient, timeout: Duration) -> Result<CmdOutcome> {
    let result = client.call("adapter.live", json!({}), timeout)?;
    Ok(CmdOutcome::Json(result))
}

fn session_attach(
    spec: AttachSpec,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    match spec {
        AttachSpec::One(id) => {
            // Verify the adapter exists server-side and learn its plugin
            // name in one round-trip. `adapter.state` is cheap and returns
            // the full classified state too, which the tab strip uses.
            let state_resp = client.call("adapter.state", json!({ "adapter": id }), timeout)?;
            let plugin = state_resp
                .get("plugin")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            ctx.upsert_adapter(&id, &plugin);
            if let Some(label) = state_resp
                .get("state")
                .and_then(|s| s.get("state"))
                .and_then(Value::as_str)
            {
                ctx.set_state_label(&id, Some(label.to_string()));
            }
            ctx.focus = Some(id.clone());
            // Auto-render the current screen so attach feels like tmux
            // attach: you immediately see what the agent is doing.
            let snap_val = client.call(
                "adapter.snapshot",
                json!({ "adapter": id, "redact": false }),
                timeout,
            )?;
            let snapshot: crate::screen::ScreenSnapshot = serde_json::from_value(snap_val)
                .map_err(|error| Error::Rpc(format!("decode adapter.snapshot: {error}")))?;
            Ok(CmdOutcome::Screen {
                adapter: id,
                snapshot,
            })
        }
        AttachSpec::All => {
            let live = client.call("adapter.live", json!({}), timeout)?;
            let adapters = live
                .get("adapters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut attached = Vec::new();
            for entry in adapters {
                if entry
                    .get("finished")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                let id = match entry.get("adapter").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => continue,
                };
                let plugin = entry
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                ctx.upsert_adapter(&id, &plugin);
                attached.push(id);
            }
            if let Some(last) = attached.last() {
                ctx.focus = Some(last.clone());
            }
            let line = if attached.is_empty() {
                "(no live adapters on the server)".to_string()
            } else {
                format!(
                    "attached {} adapter(s): {} (focus → {})",
                    attached.len(),
                    attached.join(", "),
                    attached.last().cloned().unwrap_or_default(),
                )
            };
            Ok(CmdOutcome::Line(line))
        }
    }
}

fn session_close(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let adapter = if let Some(first) = call.positional.first() {
        match first {
            Arg::String(s) => s.clone(),
            other => {
                return Err(Error::Rpc(format!(
                    "session.close expects a string adapter id; got {other}"
                )));
            }
        }
    } else {
        focus_or_err(ctx)?.to_string()
    };
    let result = client.call("adapter.close", json!({ "adapter": adapter }), timeout)?;
    ctx.remove_adapter(&adapter);
    Ok(CmdOutcome::Note {
        note: crate::repl::notes::closed(&adapter),
        value: result,
    })
}

fn send_intent(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let intent = expect_one_string(&call, "send.intent")?;
    let params = kwargs_to_json(&call.kwargs);
    let value = invoke_intent(client, ctx, &intent, params, timeout)?;
    Ok(CmdOutcome::Note {
        note: crate::repl::notes::send_intent(&intent, &value),
        value,
    })
}

/// Wire payload for `send.text("…")`. The claude-code plugin's
/// `send_prompt` intent reads `input.prompt`; sending `{"text": …}`
/// would silently bracketed-paste an empty string. Keeping this in
/// its own helper makes the wire field name a contract that a unit
/// test pins down rather than something easy to misname inside the
/// dispatcher.
fn send_text_payload(text: String) -> (&'static str, Value) {
    ("send_prompt", json!({ "prompt": text }))
}

/// Wire payload for `send.key("…")`. Mirrors `send_text_payload` for
/// the plugin's generic `key` intent, which reads `input.key`.
fn send_key_payload(key: String) -> (&'static str, Value) {
    ("key", json!({ "key": key }))
}

/// Send a named intent to the focused adapter and return the raw RPC
/// response. Callers wrap the response in a [`CmdOutcome::Note`] with
/// the appropriate per-command formatter (see [`crate::repl::notes`]).
///
/// The intermediate `Result<Value>` shape exists because the note
/// content differs by command — `send.text` reports a byte count,
/// `send.key` reports the key name, `send.intent` reports the intent
/// name — and we want the wire-format details to live next to the
/// dispatcher entry that knows them.
fn invoke_intent(
    client: &RpcClient,
    ctx: &ReplCtx,
    intent: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let adapter = focus_or_err(ctx)?.to_string();
    client.call(
        "adapter.send",
        json!({
            "adapter": adapter,
            "intent": intent,
            "params": params,
        }),
        timeout,
    )
}

/// `turn("send_prompt", prompt="...", wait=matches(r"…"), timeout=5s)` —
/// dispatch the atomic [`adapter.turn`] convenience.
///
/// Positional[0] = the send intent name. All remaining kwargs flow into
/// `send.params`, except for the three reserved wait kwargs:
///
/// * `wait=<matcher_call>` — same shape as the `wait()` DSL (e.g.
///   `matches(r"…")` or `screen_stable(250ms)`). Translates into the
///   matcher's intent + params.
/// * `wait_intent="<name>"` — override the matcher function (default
///   `wait_turn_matcher`). Use this when a plugin owns multiple matchers
///   and the caller wants one explicitly.
/// * `timeout=<duration>` — wait-leg timeout; defaults to the server's
///   120 s ceiling when omitted.
fn turn_command(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let intent = expect_one_string(&call, "turn")?;
    let adapter = focus_or_err(ctx)?.to_string();

    // Build send.params from kwargs, skipping the three wait-only keys.
    let mut send_kwargs: BTreeMap<String, Arg> = BTreeMap::new();
    for (key, value) in &call.kwargs {
        if matches!(key.as_str(), "wait" | "wait_intent" | "timeout") {
            continue;
        }
        send_kwargs.insert(key.clone(), value.clone());
    }
    let send_params = kwargs_to_json(&send_kwargs);

    // Build the wait sub-object.
    let mut wait_obj = Map::new();
    if let Some(wait_arg) = call.kwargs.get("wait") {
        let (wait_intent_resolved, wait_params) = matcher_to_params(wait_arg.clone())?;
        wait_obj.insert("intent".to_string(), Value::String(wait_intent_resolved));
        wait_obj.insert("params".to_string(), wait_params);
    } else if let Some(wait_intent_arg) = call.kwargs.get("wait_intent") {
        let wait_intent = match wait_intent_arg {
            Arg::String(s) => s.clone(),
            other => {
                return Err(Error::Rpc(format!(
                    "wait_intent= expected a string, got {other}"
                )));
            }
        };
        wait_obj.insert("intent".to_string(), Value::String(wait_intent));
    }
    if let Some(timeout_override) = duration_kwarg(&call, "timeout")? {
        // A `u128 -> u64` saturate here would silently turn a malformed
        // multi-day timeout into `u64::MAX` and the wait leg would hold
        // the per-adapter mutex for the entire (unbounded) duration —
        // which is exactly the head-of-line blocking trap adapter.turn
        // warns against. Reject the overflow explicitly so the caller
        // hears about the bad duration instead of sleeping forever.
        let millis = u64::try_from(timeout_override.as_millis()).map_err(|_| {
            Error::Rpc(format!(
                "turn(timeout=…) is too large to encode as milliseconds: {:?}",
                timeout_override
            ))
        })?;
        wait_obj.insert("timeout_ms".to_string(), Value::from(millis));
    }

    let mut params = Map::new();
    params.insert("adapter".to_string(), Value::String(adapter));
    params.insert(
        "send".to_string(),
        json!({ "intent": intent, "params": send_params }),
    );
    if !wait_obj.is_empty() {
        params.insert("wait".to_string(), Value::Object(wait_obj));
    }
    let started = std::time::Instant::now();
    let result = client.call("adapter.turn", Value::Object(params), timeout)?;
    let elapsed = started.elapsed();
    Ok(CmdOutcome::Note {
        note: crate::repl::notes::turn_complete(elapsed, &result),
        value: result,
    })
}

fn wait_command(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    // `wait(matcher, timeout=2s, wait_id="…")` — the first positional
    // arg is either a nested call (matches/screen_stable/...) or a
    // regex/duration shortcut. Optional `wait_id` makes the wait
    // cancellable via `cancel_wait("…")` from another REPL session.
    let matcher_arg = call.positional.first().cloned().ok_or_else(|| {
        Error::Rpc(
            "wait() expects a matcher: wait(matches(r\"…\")) or wait(screen_stable(250ms))"
                .to_string(),
        )
    })?;
    let timeout_arg = duration_kwarg(&call, "timeout")?;
    let wait_id = string_kwarg(&call, "wait_id");
    let (intent, params) = matcher_to_params(matcher_arg)?;
    let (elapsed, value) = invoke_wait(
        client,
        ctx,
        &intent,
        params.clone(),
        timeout_arg,
        wait_id,
        timeout,
    )?;
    Ok(CmdOutcome::Note {
        note: note_for_wait(&params, elapsed, &value),
        value,
    })
}

fn wait_with_params(
    client: &RpcClient,
    ctx: &mut ReplCtx,
    intent: &str,
    matcher_params: Value,
    timeout_override: Option<Duration>,
    fallback_timeout: Duration,
) -> Result<CmdOutcome> {
    let (elapsed, value) = invoke_wait(
        client,
        ctx,
        intent,
        matcher_params.clone(),
        timeout_override,
        None,
        fallback_timeout,
    )?;
    Ok(CmdOutcome::Note {
        note: note_for_wait(&matcher_params, elapsed, &value),
        value,
    })
}

/// Pick the correct [`crate::repl::notes`] formatter for a `wait()` call
/// based on the params we sent. The intent is always
/// `wait_turn_matcher`; the params discriminator (`pattern` vs
/// `stable_ms`) is what distinguishes a regex match from a
/// screen-stable wait. Anything else falls back to a generic
/// `wait_stable` shape — that's the closest visual analogue and avoids
/// adding a third note variant that would only ever fire for plugin
/// matchers we don't have today.
fn note_for_wait(params: &Value, elapsed: Duration, response: &Value) -> crate::repl::notes::Note {
    if params.get("pattern").is_some() {
        crate::repl::notes::wait_match(elapsed, response)
    } else {
        crate::repl::notes::wait_stable(elapsed, response)
    }
}

/// Issue the `adapter.wait` RPC and return `(elapsed, value)`. Callers
/// build the [`CmdOutcome::Note`] with the appropriate formatter. The
/// elapsed time is measured around the blocking call so the operator
/// sees wall-clock latency, not just whatever timestamp the server
/// reports.
fn invoke_wait(
    client: &RpcClient,
    ctx: &mut ReplCtx,
    intent: &str,
    matcher_params: Value,
    timeout_override: Option<Duration>,
    wait_id: Option<String>,
    fallback_timeout: Duration,
) -> Result<(Duration, Value)> {
    let adapter = focus_or_err(ctx)?.to_string();
    let mut req = Map::new();
    req.insert("adapter".to_string(), Value::String(adapter));
    req.insert("intent".to_string(), Value::String(intent.to_string()));
    req.insert("params".to_string(), matcher_params);
    if let Some(t) = timeout_override {
        req.insert(
            "timeout_ms".to_string(),
            Value::from(u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        );
    }
    if let Some(id) = wait_id {
        req.insert("wait_id".to_string(), Value::String(id));
    }
    let started = std::time::Instant::now();
    let result = client.call("adapter.wait", Value::Object(req), fallback_timeout)?;
    Ok((started.elapsed(), result))
}

/// REPL DSL: `cancel_wait("wait-id-1")` — issues `adapter.cancel_wait
/// { wait_id }` against the server. Useful when a previously-issued
/// `wait(..., wait_id="…")` is still in flight (typically from another
/// REPL session or background script) and the operator wants to break
/// it loose.
fn cancel_wait_command(call: DslCall, client: &RpcClient, timeout: Duration) -> Result<CmdOutcome> {
    let wait_id = expect_one_string(&call, "cancel_wait")?;
    let result = client.call(
        "adapter.cancel_wait",
        json!({ "wait_id": wait_id }),
        timeout,
    )?;
    Ok(CmdOutcome::Json(result))
}

fn matcher_to_params(arg: Arg) -> Result<(String, Value)> {
    match arg {
        Arg::Regex(pattern) => Ok((
            "wait_turn_matcher".to_string(),
            json!({ "pattern": pattern }),
        )),
        Arg::Duration(stable) => Ok((
            "wait_turn_matcher".to_string(),
            json!({ "stable_ms": stable.as_millis() }),
        )),
        Arg::Call(inner) => match inner.path.join(".").as_str() {
            "matches" => {
                let pattern = expect_one_regex(&inner, "matches")?;
                Ok((
                    "wait_turn_matcher".to_string(),
                    json!({ "pattern": pattern }),
                ))
            }
            "screen_stable" => {
                let stable = expect_one_duration(&inner, "screen_stable")?;
                Ok((
                    "wait_turn_matcher".to_string(),
                    json!({ "stable_ms": stable.as_millis() }),
                ))
            }
            other => Err(Error::Rpc(format!(
                "wait() does not know matcher `{other}` — try matches(r\"…\") or screen_stable(250ms)"
            ))),
        },
        other => Err(Error::Rpc(format!(
            "wait() expected a matcher, got {other}"
        ))),
    }
}

// ---- arg helpers -------------------------------------------------------

fn expect_one_string(call: &DslCall, label: &str) -> Result<String> {
    let arg = call
        .positional
        .first()
        .ok_or_else(|| Error::Rpc(format!("{label} expects a string argument")))?;
    match arg {
        Arg::String(s) => Ok(s.clone()),
        other => Err(Error::Rpc(format!(
            "{label} expected a string argument, got {other}"
        ))),
    }
}

fn expect_one_regex(call: &DslCall, label: &str) -> Result<String> {
    let arg = call
        .positional
        .first()
        .ok_or_else(|| Error::Rpc(format!("{label} expects a regex argument: {label}(r\"…\")")))?;
    match arg {
        Arg::Regex(s) => Ok(s.clone()),
        Arg::String(s) => Ok(s.clone()),
        other => Err(Error::Rpc(format!(
            "{label} expected a regex/string argument, got {other}"
        ))),
    }
}

fn expect_one_duration(call: &DslCall, label: &str) -> Result<Duration> {
    let arg = call
        .positional
        .first()
        .ok_or_else(|| Error::Rpc(format!("{label} expects a duration: {label}(250ms)")))?;
    match arg {
        Arg::Duration(d) => Ok(*d),
        other => Err(Error::Rpc(format!(
            "{label} expected a duration argument, got {other}"
        ))),
    }
}

fn string_kwarg(call: &DslCall, name: &str) -> Option<String> {
    match call.kwargs.get(name)? {
        Arg::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn bool_kwarg(call: &DslCall, name: &str) -> Option<bool> {
    match call.kwargs.get(name)? {
        Arg::Bool(b) => Some(*b),
        _ => None,
    }
}

fn duration_kwarg(call: &DslCall, name: &str) -> Result<Option<Duration>> {
    match call.kwargs.get(name) {
        None => Ok(None),
        Some(Arg::Duration(d)) => Ok(Some(*d)),
        Some(other) => Err(Error::Rpc(format!(
            "{name}= expected a duration, got {other}"
        ))),
    }
}

fn prior_adapter_kwarg(call: &DslCall) -> Result<Option<String>> {
    if let Some(arg) = call.kwargs.get("prior_adapter") {
        return match arg {
            Arg::String(s) => Ok(Some(s.clone())),
            other => Err(Error::Rpc(format!(
                "prior_adapter= expected a string, got {other}"
            ))),
        };
    }
    if let Some(arg) = call.kwargs.get("prior") {
        return match arg {
            Arg::String(s) => Ok(Some(s.clone())),
            other => Err(Error::Rpc(format!("prior= expected a string, got {other}"))),
        };
    }
    Ok(None)
}

fn insert_adapter_start_kwargs(params: &mut Map<String, Value>, call: &DslCall) -> Result<()> {
    if let Some(program) = string_kwarg(call, "program") {
        params.insert("program".to_string(), Value::String(program));
    }
    if let Some(args) = string_list_kwarg(call, "args")? {
        params.insert(
            "args".to_string(),
            Value::Array(args.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(cwd) = string_kwarg(call, "cwd") {
        params.insert("cwd".to_string(), Value::String(cwd));
    }
    if let Some(env) = string_map_kwarg(call, "env")? {
        params.insert(
            "env".to_string(),
            Value::Object(
                env.into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            ),
        );
    }
    for name in ["rows", "cols", "pixel_width", "pixel_height"] {
        if let Some(value) = u16_kwarg(call, name)? {
            params.insert(name.to_string(), Value::from(value));
        }
    }
    Ok(())
}

fn string_list_kwarg(call: &DslCall, name: &str) -> Result<Option<Vec<String>>> {
    let Some(arg) = call.kwargs.get(name) else {
        return Ok(None);
    };
    let Arg::List(values) = arg else {
        return Err(Error::Rpc(format!(
            "{name}= expected a string list, got {arg}"
        )));
    };
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Arg::String(s) => out.push(s.clone()),
            other => {
                return Err(Error::Rpc(format!(
                    "{name}= expected a string list, got item {other}"
                )));
            }
        }
    }
    Ok(Some(out))
}

fn string_map_kwarg(call: &DslCall, name: &str) -> Result<Option<BTreeMap<String, String>>> {
    let Some(arg) = call.kwargs.get(name) else {
        return Ok(None);
    };
    let Arg::Object(values) = arg else {
        return Err(Error::Rpc(format!(
            "{name}= expected a string object, got {arg}"
        )));
    };
    let mut out = BTreeMap::new();
    for (key, value) in values {
        match value {
            Arg::String(s) => {
                out.insert(key.clone(), s.clone());
            }
            other => {
                return Err(Error::Rpc(format!(
                    "{name}= expected string values, got `{key}`={other}"
                )));
            }
        }
    }
    Ok(Some(out))
}

fn u16_kwarg(call: &DslCall, name: &str) -> Result<Option<u16>> {
    match call.kwargs.get(name) {
        None => Ok(None),
        Some(Arg::Int(n)) => u16::try_from(*n)
            .map(Some)
            .map_err(|_| Error::Rpc(format!("{name}= expected 0..65535, got {n}"))),
        Some(other) => Err(Error::Rpc(format!(
            "{name}= expected an integer, got {other}"
        ))),
    }
}

fn kwargs_to_json(kwargs: &BTreeMap<String, Arg>) -> Value {
    let mut out = Map::new();
    for (k, v) in kwargs {
        out.insert(k.clone(), arg_to_json(v));
    }
    Value::Object(out)
}

fn arg_to_json(arg: &Arg) -> Value {
    match arg {
        Arg::String(s) => Value::String(s.clone()),
        Arg::Regex(s) => Value::String(s.clone()),
        Arg::Duration(d) => Value::from(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        Arg::Int(n) => Value::from(*n),
        Arg::Bool(b) => Value::Bool(*b),
        Arg::Null => Value::Null,
        Arg::List(values) => Value::Array(values.iter().map(arg_to_json).collect()),
        Arg::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), arg_to_json(value)))
                .collect(),
        ),
        Arg::Call(call) => Value::String(format!("{}(...)", call.path.join("."))),
    }
}

pub fn help_text() -> &'static str {
    "ptywright repl commands:\n\
     \n\
     Sessions:\n\
       plugins()                       list built-in plugins\n\
       plugins.describe(\"name\")        return a plugin's intents / matchers / states\n\
       session.spawn(\"name\")         spawn an adapter for the named plugin\n\
       session.resume(\"name\", prior_adapter=\"id\") resume and close prior id (alias: prior)\n\
       session.list()                  list known adapters (this REPL)\n\
       session.live()                  list adapters live on the server\n\
       session.attach(\"id\")          attach a server adapter into this REPL\n\
       session.attach(\"all\")          attach every live adapter\n\
       session.close(id?)              close the focused (or named) adapter\n\
       state()                         re-classify the focused adapter\n\
     \n\
     Driving the focused adapter:\n\
       send.text(\"…\")                 send a prompt\n\
       send.key(\"y\")                  send a single key\n\
       send.intent(\"name\", k=v, …)   invoke a plugin intent\n\
       turn(\"intent\", k=v, …)         atomic send + wait_turn_matcher (adapter.turn)\n\
       turn(\"intent\", k=v, wait=matches(r\"…\"), timeout=5s)\n\
       wait(matches(r\"…\"))            wait for output to match\n\
       wait(screen_stable(250ms))      wait for the screen to settle\n\
       transcript.snapshot(redact=true)\n\
       screen.snapshot()                render the focused PTY inline\n\
       view()                           alias for screen.snapshot()\n\
       inspect()                        diagnostic dump\n\
     \n\
     Meta:\n\
       :tabs       :focus <id>   :live   :attach <id|all>\n\
       :notifications on|off [adapters=…] [sessions=…]\n\
       :rpc <method> {json}\n\
       :quit       :help\n"
}

// ---- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::Framing;
    use crate::repl::transport::RpcClient;
    use crate::rpc::{RpcServerState, serve_ndjson_with_state};
    use std::io::pipe;

    // ---- parser tests --------------------------------------------------

    #[test]
    fn parse_bare_call() {
        let cmd = parse("plugins()").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.path, vec!["plugins"]);
        assert!(call.positional.is_empty());
        assert!(call.kwargs.is_empty());
    }

    #[test]
    fn parse_dotted_path() {
        let cmd = parse("session.spawn(\"claude-code\")").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.path, vec!["session", "spawn"]);
        assert_eq!(call.positional, vec![Arg::String("claude-code".into())]);
    }

    #[test]
    fn parse_string_with_escapes() {
        let cmd = parse(r#"send.text("hello\n\"world\"")"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(
            call.positional,
            vec![Arg::String("hello\n\"world\"".into())]
        );
    }

    #[test]
    fn parse_regex_literal() {
        let cmd = parse(r#"wait.matches(r"^Approve\? \(y/n\)")"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(
            call.positional,
            vec![Arg::Regex("^Approve\\? \\(y/n\\)".into())]
        );
    }

    #[test]
    fn parse_duration_units() {
        for (input, expected) in [
            ("wait.screen_stable(250ms)", Duration::from_millis(250)),
            ("wait.screen_stable(2s)", Duration::from_secs(2)),
        ] {
            let cmd = parse(input).unwrap();
            let Cmd::Dsl(call) = cmd else {
                panic!("{input}")
            };
            assert_eq!(call.positional, vec![Arg::Duration(expected)]);
        }
    }

    #[test]
    fn parse_integer_argument() {
        let cmd = parse("session.spawn(\"x\", rows=24, cols=80)").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.kwargs.get("rows"), Some(&Arg::Int(24)));
        assert_eq!(call.kwargs.get("cols"), Some(&Arg::Int(80)));
    }

    #[test]
    fn parse_lists_and_objects_for_start_params() {
        let cmd = parse(
            r#"session.spawn("claude-code", args=["--model", "haiku"], env={NO_COLOR:"1", "X_FLAG"="yes"})"#,
        )
        .unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(
            call.kwargs.get("args"),
            Some(&Arg::List(vec![
                Arg::String("--model".into()),
                Arg::String("haiku".into())
            ]))
        );

        let mut expected_env = BTreeMap::new();
        expected_env.insert("NO_COLOR".to_string(), Arg::String("1".into()));
        expected_env.insert("X_FLAG".to_string(), Arg::String("yes".into()));
        assert_eq!(call.kwargs.get("env"), Some(&Arg::Object(expected_env)));
    }

    #[test]
    fn parse_booleans_and_null() {
        let cmd = parse("transcript.snapshot(redact=false)").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.kwargs.get("redact"), Some(&Arg::Bool(false)));

        let cmd = parse("send.intent(\"x\", foo=true, bar=null)").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.kwargs.get("foo"), Some(&Arg::Bool(true)));
        assert_eq!(call.kwargs.get("bar"), Some(&Arg::Null));
    }

    #[test]
    fn parse_nested_call() {
        let cmd = parse(r#"wait(matches(r"^Total cost:"))"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.path, vec!["wait"]);
        match &call.positional[0] {
            Arg::Call(inner) => {
                assert_eq!(inner.path, vec!["matches"]);
                assert_eq!(inner.positional, vec![Arg::Regex("^Total cost:".into())]);
            }
            other => panic!("expected nested call, got {other:?}"),
        }
    }

    #[test]
    fn parse_meta_help() {
        assert_eq!(parse(":help").unwrap(), Cmd::Meta(MetaCmd::Help));
    }

    #[test]
    fn parse_meta_quit_aliases() {
        for alias in [":quit", ":q", ":exit"] {
            assert_eq!(parse(alias).unwrap(), Cmd::Meta(MetaCmd::Quit));
        }
    }

    #[test]
    fn parse_meta_tabs() {
        assert_eq!(parse(":tabs").unwrap(), Cmd::Meta(MetaCmd::Tabs));
    }

    #[test]
    fn parse_meta_focus() {
        assert_eq!(
            parse(":focus e2").unwrap(),
            Cmd::Meta(MetaCmd::Focus("e2".into()))
        );
    }

    #[test]
    fn parse_meta_focus_requires_argument() {
        let error = parse(":focus").unwrap_err();
        assert!(error.to_string().contains(":focus"), "{error}");
    }

    #[test]
    fn parse_meta_notifications_on_off() {
        assert_eq!(
            parse(":notifications on").unwrap(),
            Cmd::Meta(MetaCmd::Notifications(NotificationsRequest::simple(true)))
        );
        assert_eq!(
            parse(":notifications off").unwrap(),
            Cmd::Meta(MetaCmd::Notifications(NotificationsRequest::simple(false)))
        );
    }

    #[test]
    fn parse_meta_notifications_rejects_garbage() {
        let error = parse(":notifications maybe").unwrap_err();
        assert!(error.to_string().contains("on|off"), "{error}");
    }

    #[test]
    fn parse_meta_notifications_accepts_filter_kwargs() {
        // `adapters=` / `sessions=` scope the per-stream filter that the
        // server applies before fanning notifications out. Comma-separated,
        // whitespace-trimmed, empty entries dropped.
        let parsed = parse(":notifications on adapters=e1,e2 sessions=s1").unwrap();
        let Cmd::Meta(MetaCmd::Notifications(request)) = parsed else {
            panic!("expected MetaCmd::Notifications, got {parsed:?}");
        };
        assert!(request.enabled);
        assert_eq!(request.adapters, vec!["e1".to_string(), "e2".to_string()]);
        assert_eq!(request.sessions, vec!["s1".to_string()]);
    }

    #[test]
    fn parse_meta_notifications_rejects_filter_when_disabled() {
        // The filter is meaningless without an active subscription —
        // require callers to drop it when turning notifications off so
        // the wire shape stays self-consistent.
        let error = parse(":notifications off adapters=e1").unwrap_err();
        assert!(
            error.to_string().contains("off does not take filter args"),
            "{error}"
        );
    }

    #[test]
    fn parse_meta_notifications_rejects_unknown_filter_token() {
        let error = parse(":notifications on widgets=foo").unwrap_err();
        assert!(error.to_string().contains("widgets=foo"), "{error}");
    }

    #[test]
    fn parse_meta_notifications_accepts_comma_space_separated_lists() {
        // Operators reach for `adapters=e1, e2` (comma + space) by
        // muscle memory — `split_whitespace()` alone would tokenize
        // `e2` as a stray bareword. The parser must treat a bare
        // continuation token as more ids for the previous filter list.
        let parsed = parse(":notifications on adapters=e1, e2 sessions=s1, s2").unwrap();
        let Cmd::Meta(MetaCmd::Notifications(request)) = parsed else {
            panic!("expected MetaCmd::Notifications, got {parsed:?}");
        };
        assert!(request.enabled);
        assert_eq!(request.adapters, vec!["e1".to_string(), "e2".to_string()]);
        assert_eq!(request.sessions, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[test]
    fn parse_meta_live() {
        assert_eq!(parse(":live").unwrap(), Cmd::Meta(MetaCmd::Live));
    }

    #[test]
    fn parse_meta_attach_specific_id() {
        let cmd = parse(":attach e1").unwrap();
        assert_eq!(
            cmd,
            Cmd::Meta(MetaCmd::Attach(AttachSpec::One("e1".into())))
        );
    }

    #[test]
    fn parse_meta_attach_all() {
        let cmd = parse(":attach all").unwrap();
        assert_eq!(cmd, Cmd::Meta(MetaCmd::Attach(AttachSpec::All)));
    }

    #[test]
    fn parse_meta_attach_requires_argument() {
        let error = parse(":attach").unwrap_err();
        assert!(error.to_string().contains(":attach"), "{error}");
    }

    #[test]
    fn parse_meta_rpc_with_json_params() {
        let cmd = parse(r#":rpc adapter.send {"adapter":"e1","intent":"approve"}"#).unwrap();
        let Cmd::Meta(MetaCmd::Rpc { method, params }) = cmd else {
            panic!()
        };
        assert_eq!(method, "adapter.send");
        assert_eq!(params["adapter"], "e1");
        assert_eq!(params["intent"], "approve");
    }

    #[test]
    fn parse_meta_rpc_defaults_to_empty_params() {
        let cmd = parse(":rpc adapter.list").unwrap();
        let Cmd::Meta(MetaCmd::Rpc { method, params }) = cmd else {
            panic!()
        };
        assert_eq!(method, "adapter.list");
        assert!(params.is_object());
    }

    #[test]
    fn parse_unknown_meta() {
        let error = parse(":teleport").unwrap_err();
        assert!(error.to_string().contains("teleport"), "{error}");
    }

    #[test]
    fn parse_empty_input_errors() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn parse_unterminated_string_errors() {
        let error = parse(r#"send.text("hello"#).unwrap_err();
        assert!(error.to_string().contains("unterminated"), "{error}");
    }

    #[test]
    fn parse_unterminated_paren_errors() {
        let error = parse(r#"send.text("hi""#).unwrap_err();
        assert!(error.to_string().to_lowercase().contains("expected"));
    }

    #[test]
    fn parse_unknown_duration_unit_errors() {
        let error = parse(r#"wait.screen_stable(5min)"#).unwrap_err();
        assert!(error.to_string().contains("duration unit"), "{error}");
    }

    #[test]
    fn parse_trailing_garbage_errors() {
        let error = parse("plugins() extra").unwrap_err();
        assert!(error.to_string().contains("trailing"), "{error}");
    }

    // ---- dispatcher tests ----------------------------------------------

    fn in_process_client() -> (
        std::sync::Arc<RpcClient>,
        std::thread::JoinHandle<()>,
        ReplCtx,
    ) {
        let (c2s_r, c2s_w) = pipe().expect("c2s pipe");
        let (s2c_r, s2c_w) = pipe().expect("s2c pipe");
        let state = RpcServerState::new();
        let server = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
        });
        let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
        (client, server, ReplCtx::new())
    }

    #[test]
    fn dispatch_plugins_returns_builtin_list() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse("plugins()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("dispatch plugins");
        match outcome {
            CmdOutcome::Json(value) => {
                assert!(value["plugins"].is_array());
            }
            other => panic!("expected json outcome, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_session_spawn_focuses_returned_adapter() {
        let (client, _server, mut ctx) = in_process_client();
        let cmd = parse(r#"session.spawn("claude-code", program="/bin/sh")"#).expect("parse spawn");
        let outcome =
            dispatch(cmd, &client, &mut ctx, Duration::from_secs(5)).expect("spawn dispatch");
        let CmdOutcome::Note { value, .. } = outcome else {
            panic!("expected note outcome from spawn, got {outcome:?}")
        };
        let adapter = value["adapter"].as_str().expect("adapter id");
        assert_eq!(ctx.focus.as_deref(), Some(adapter));
        assert!(ctx.adapter(adapter).is_some());

        // Close it back out so the child PTY exits cleanly.
        let _ = dispatch(
            parse("session.close()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        );
        assert!(ctx.adapters.is_empty());
        assert!(ctx.focus.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_session_resume_replaces_prior_adapter_and_focuses_new_one() {
        let (client, _server, mut ctx) = in_process_client();
        let first = dispatch(
            parse(r#"session.spawn("claude-code", program="/bin/sh", args=["-c", "sleep 5"])"#)
                .unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("spawn prior");
        let CmdOutcome::Note {
            value: first_value, ..
        } = first
        else {
            panic!("expected note outcome from spawn, got {first:?}")
        };
        let prior = first_value["adapter"]
            .as_str()
            .expect("prior adapter")
            .to_string();

        let resumed = dispatch(
            parse(&format!(
                r#"session.resume("claude-code", program="/bin/sh", args=["-c", "sleep 5"], prior_adapter="{prior}")"#
            ))
            .unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("resume");
        let CmdOutcome::Note {
            value: resumed_value,
            ..
        } = resumed
        else {
            panic!("expected note outcome from resume, got {resumed:?}")
        };
        let replacement = resumed_value["adapter"]
            .as_str()
            .expect("replacement adapter");
        assert_ne!(prior, replacement);
        assert_eq!(ctx.focus.as_deref(), Some(replacement));
        assert!(ctx.adapter(&prior).is_none(), "prior tab must be removed");
        assert!(
            ctx.adapter(replacement).is_some(),
            "replacement tab missing"
        );

        let _ = dispatch(
            parse("session.close()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        );
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_session_resume_accepts_prior_alias() {
        let (client, _server, mut ctx) = in_process_client();
        let first = dispatch(
            parse(r#"session.spawn("claude-code", program="/bin/sh", args=["-c", "sleep 5"])"#)
                .unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("spawn prior");
        let CmdOutcome::Note {
            value: first_value, ..
        } = first
        else {
            panic!("expected note outcome from spawn, got {first:?}")
        };
        let prior = first_value["adapter"]
            .as_str()
            .expect("prior adapter")
            .to_string();

        let resumed = dispatch(
            parse(&format!(
                r#"session.resume("claude-code", program="/bin/sh", args=["-c", "sleep 5"], prior="{prior}")"#
            ))
            .unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("resume with prior alias");
        let CmdOutcome::Note {
            value: resumed_value,
            ..
        } = resumed
        else {
            panic!("expected note outcome from resume, got {resumed:?}")
        };
        let replacement = resumed_value["adapter"]
            .as_str()
            .expect("replacement adapter");
        assert_ne!(prior, replacement);
        assert!(ctx.adapter(&prior).is_none(), "prior tab must be removed");
        assert_eq!(ctx.focus.as_deref(), Some(replacement));

        let _ = dispatch(
            parse("session.close()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        );
    }

    #[test]
    fn dispatch_meta_tabs_renders_focus_marker() {
        let (client, _server, mut ctx) = in_process_client();
        ctx.upsert_adapter("e1", "claude-code");
        ctx.upsert_adapter("e2", "claude-code");
        ctx.focus = Some("e2".into());
        let outcome = dispatch(
            parse(":tabs").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Line(line) = outcome else {
            panic!()
        };
        assert!(line.contains("*e2"), "expected `*e2`, got `{line}`");
    }

    #[test]
    fn dispatch_quit_returns_quit_outcome() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse(":quit").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(outcome, CmdOutcome::Quit));
    }

    #[test]
    fn dispatch_rpc_passthrough_invokes_server() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse(":rpc adapter.list").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Json(value) = outcome else {
            panic!()
        };
        assert!(value["plugins"].is_array());
    }

    #[test]
    fn send_text_payload_uses_prompt_wire_field() {
        // Regression: previously the dispatcher sent `params: {text: …}`
        // but the claude-code plugin reads `input.prompt`. Locking the
        // helper output ensures a future rename of the DSL form
        // doesn't silently drop the plugin contract.
        let (intent, params) = send_text_payload("hello".to_string());
        assert_eq!(intent, "send_prompt");
        assert_eq!(params, json!({ "prompt": "hello" }));
        assert!(params.get("text").is_none(), "must not use legacy `text`");
    }

    #[test]
    fn send_key_payload_uses_key_wire_field() {
        let (intent, params) = send_key_payload("enter".to_string());
        assert_eq!(intent, "key");
        assert_eq!(params, json!({ "key": "enter" }));
    }

    #[test]
    fn dispatch_session_live_returns_server_list() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse("session.live()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("session.live dispatch");
        match outcome {
            CmdOutcome::Json(value) => assert!(value["adapters"].is_array()),
            other => panic!("expected json outcome, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_session_attach_all_pulls_running_adapters() {
        let (client, _server, mut ctx_seed) = in_process_client();
        // Spawn one adapter through the same server, then forget about it
        // locally so the next ctx has to discover it via attach.
        let _ = dispatch(
            parse(r#"session.spawn("claude-code", program="/bin/sh")"#).unwrap(),
            &client,
            &mut ctx_seed,
            Duration::from_secs(5),
        )
        .expect("seed spawn");

        let mut fresh = ReplCtx::new();
        let outcome = dispatch(
            parse(":attach all").unwrap(),
            &client,
            &mut fresh,
            Duration::from_secs(5),
        )
        .expect("attach all");
        let CmdOutcome::Line(line) = outcome else {
            panic!("expected Line outcome from :attach all")
        };
        assert!(line.contains("attached"), "{line}");
        assert!(!fresh.adapters.is_empty(), "no adapters were attached");
        assert!(fresh.focus.is_some(), "focus was not set");

        // Close it through whichever ctx still has the id so the PTY exits.
        let id = fresh.adapters[0].id.clone();
        let _ = dispatch(
            parse(&format!("session.close(\"{id}\")")).unwrap(),
            &client,
            &mut fresh,
            Duration::from_secs(5),
        );
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_session_attach_specific_id_returns_screen() {
        let (client, _server, mut ctx_seed) = in_process_client();
        let spawn = dispatch(
            parse(r#"session.spawn("claude-code", program="/bin/sh")"#).unwrap(),
            &client,
            &mut ctx_seed,
            Duration::from_secs(5),
        )
        .expect("seed spawn");
        let CmdOutcome::Note { value, .. } = spawn else {
            panic!("expected note outcome from spawn, got {spawn:?}")
        };
        let id = value["adapter"].as_str().expect("adapter id").to_string();

        let mut fresh = ReplCtx::new();
        let outcome = dispatch(
            parse(&format!(":attach {id}")).unwrap(),
            &client,
            &mut fresh,
            Duration::from_secs(5),
        )
        .expect("attach single");
        let CmdOutcome::Screen { adapter, .. } = outcome else {
            panic!("expected Screen outcome from :attach <id>")
        };
        assert_eq!(adapter, id);
        assert_eq!(fresh.focus.as_deref(), Some(id.as_str()));
        assert!(fresh.adapter(&id).is_some());

        let _ = dispatch(
            parse("session.close()").unwrap(),
            &client,
            &mut fresh,
            Duration::from_secs(5),
        );
    }

    #[test]
    fn dispatch_session_attach_rejects_unknown_id() {
        let (client, _server, mut ctx) = in_process_client();
        let error = dispatch(
            parse(":attach nope").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(
            text.to_lowercase().contains("adapter") || text.contains("nope"),
            "expected unknown-adapter error, got: {text}"
        );
    }

    #[test]
    fn dispatch_state_without_focus_returns_helpful_error() {
        let (client, _server, mut ctx) = in_process_client();
        let error = dispatch(
            parse("state()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no focused adapter"), "{error}");
    }

    // ---- helper-level tests (pure, no RPC client) ----------------------
    //
    // The blocks below exercise the parser-adjacent helpers and the
    // pure branches of the dispatcher that don't need a live RPC server.
    // They're cheap, deterministic, and they cover the slice of
    // `command.rs` that the in-process dispatcher tests above can't
    // reach without an actual adapter.

    fn call(path: &str, positional: Vec<Arg>) -> DslCall {
        DslCall {
            path: path.split('.').map(str::to_string).collect(),
            positional,
            kwargs: BTreeMap::new(),
        }
    }

    fn call_kw(path: &str, kwargs: Vec<(&str, Arg)>) -> DslCall {
        DslCall {
            path: path.split('.').map(str::to_string).collect(),
            positional: Vec::new(),
            kwargs: kwargs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    #[test]
    fn arg_display_formats_every_variant() {
        // `Arg::fmt` is used inside every "expected X, got Y" error
        // message, so a missing branch turns into an opaque panic at
        // runtime rather than a friendly diagnostic.
        assert_eq!(format!("{}", Arg::String("a".into())), r#""a""#);
        assert_eq!(format!("{}", Arg::Regex("a".into())), r#"r"a""#);
        assert_eq!(
            format!("{}", Arg::Duration(Duration::from_millis(250))),
            "250ms"
        );
        assert_eq!(format!("{}", Arg::Int(42)), "42");
        assert_eq!(format!("{}", Arg::Bool(true)), "true");
        assert_eq!(format!("{}", Arg::Null), "null");
        assert_eq!(format!("{}", Arg::List(vec![Arg::Int(1)])), "[1 item(s)]");
        assert_eq!(
            format!(
                "{}",
                Arg::Object([("a".to_string(), Arg::Int(1))].into_iter().collect())
            ),
            "{1 key(s)}"
        );
        assert_eq!(
            format!("{}", Arg::Call(call("matches", vec![]))),
            "matches(...)"
        );
    }

    #[test]
    fn matcher_to_params_handles_bare_regex_and_duration() {
        let (intent, params) = matcher_to_params(Arg::Regex("foo".into())).unwrap();
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["pattern"], "foo");

        let (intent, params) =
            matcher_to_params(Arg::Duration(Duration::from_millis(500))).unwrap();
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["stable_ms"], 500);
    }

    #[test]
    fn matcher_to_params_recognises_nested_matches_and_screen_stable() {
        let inner = call("matches", vec![Arg::Regex("ready".into())]);
        let (intent, params) = matcher_to_params(Arg::Call(inner)).unwrap();
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["pattern"], "ready");

        let inner = call(
            "screen_stable",
            vec![Arg::Duration(Duration::from_millis(250))],
        );
        let (intent, params) = matcher_to_params(Arg::Call(inner)).unwrap();
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["stable_ms"], 250);
    }

    #[test]
    fn matcher_to_params_rejects_unknown_call_and_wrong_arg_types() {
        let unknown = call("teleport", vec![Arg::Regex("x".into())]);
        let error = matcher_to_params(Arg::Call(unknown)).unwrap_err();
        assert!(error.to_string().contains("teleport"), "{error}");

        let error = matcher_to_params(Arg::String("hi".into())).unwrap_err();
        assert!(error.to_string().contains("matcher"), "{error}");
    }

    #[test]
    fn expect_one_string_reports_missing_and_wrong_type() {
        let empty = call("send.text", vec![]);
        let error = expect_one_string(&empty, "send.text").unwrap_err();
        assert!(error.to_string().contains("send.text"), "{error}");

        let wrong = call("send.text", vec![Arg::Int(7)]);
        let error = expect_one_string(&wrong, "send.text").unwrap_err();
        assert!(error.to_string().contains("string"), "{error}");
    }

    #[test]
    fn expect_one_regex_accepts_string_or_regex_and_rejects_others() {
        let regex = call("wait.matches", vec![Arg::Regex("foo".into())]);
        assert_eq!(expect_one_regex(&regex, "wait.matches").unwrap(), "foo");

        let string = call("wait.matches", vec![Arg::String("foo".into())]);
        assert_eq!(expect_one_regex(&string, "wait.matches").unwrap(), "foo");

        let bool_arg = call("wait.matches", vec![Arg::Bool(true)]);
        let error = expect_one_regex(&bool_arg, "wait.matches").unwrap_err();
        assert!(error.to_string().contains("regex/string"), "{error}");

        let empty = call("wait.matches", vec![]);
        let error = expect_one_regex(&empty, "wait.matches").unwrap_err();
        assert!(error.to_string().contains("regex"), "{error}");
    }

    #[test]
    fn expect_one_duration_only_accepts_durations() {
        let dur = call(
            "wait.screen_stable",
            vec![Arg::Duration(Duration::from_millis(100))],
        );
        assert_eq!(
            expect_one_duration(&dur, "wait.screen_stable").unwrap(),
            Duration::from_millis(100)
        );

        let str_arg = call("wait.screen_stable", vec![Arg::String("100".into())]);
        let error = expect_one_duration(&str_arg, "wait.screen_stable").unwrap_err();
        assert!(error.to_string().contains("duration"), "{error}");

        let empty = call("wait.screen_stable", vec![]);
        let error = expect_one_duration(&empty, "wait.screen_stable").unwrap_err();
        assert!(error.to_string().contains("duration"), "{error}");
    }

    #[test]
    fn kwarg_helpers_return_typed_values_or_none() {
        let c = call_kw(
            "session.spawn",
            vec![
                ("program", Arg::String("/bin/sh".into())),
                ("rows", Arg::Int(24)),
                ("verbose", Arg::Bool(true)),
            ],
        );
        assert_eq!(string_kwarg(&c, "program").as_deref(), Some("/bin/sh"));
        assert_eq!(bool_kwarg(&c, "verbose"), Some(true));

        // Wrong types silently return None — the dispatcher then errors
        // with a usage message rather than coercing a bool into a string.
        assert!(string_kwarg(&c, "rows").is_none());
        assert!(bool_kwarg(&c, "rows").is_none());

        // Missing key is always None.
        assert!(string_kwarg(&c, "missing").is_none());
    }

    #[test]
    fn duration_kwarg_distinguishes_missing_present_and_wrong_type() {
        let none = call_kw("wait", vec![]);
        assert_eq!(duration_kwarg(&none, "timeout").unwrap(), None);

        let dur = call_kw(
            "wait",
            vec![("timeout", Arg::Duration(Duration::from_secs(2)))],
        );
        assert_eq!(
            duration_kwarg(&dur, "timeout").unwrap(),
            Some(Duration::from_secs(2))
        );

        let wrong = call_kw("wait", vec![("timeout", Arg::Int(2000))]);
        let error = duration_kwarg(&wrong, "timeout").unwrap_err();
        assert!(error.to_string().contains("duration"), "{error}");
    }

    #[test]
    fn prior_adapter_kwarg_accepts_long_and_short_alias() {
        let long = call_kw(
            "session.resume",
            vec![("prior_adapter", Arg::String("e1".into()))],
        );
        assert_eq!(prior_adapter_kwarg(&long).unwrap().as_deref(), Some("e1"));

        let short = call_kw("session.resume", vec![("prior", Arg::String("e2".into()))]);
        assert_eq!(prior_adapter_kwarg(&short).unwrap().as_deref(), Some("e2"));

        let both = call_kw(
            "session.resume",
            vec![
                ("prior_adapter", Arg::String("e1".into())),
                ("prior", Arg::String("e2".into())),
            ],
        );
        assert_eq!(
            prior_adapter_kwarg(&both).unwrap().as_deref(),
            Some("e1"),
            "prior_adapter should win when both spellings are present"
        );

        let wrong_long = call_kw("session.resume", vec![("prior_adapter", Arg::Int(1))]);
        let error = prior_adapter_kwarg(&wrong_long).unwrap_err();
        assert!(error.to_string().contains("prior_adapter="), "{error}");

        let wrong_short = call_kw("session.resume", vec![("prior", Arg::Bool(true))]);
        let error = prior_adapter_kwarg(&wrong_short).unwrap_err();
        assert!(error.to_string().contains("prior="), "{error}");
    }

    #[test]
    fn adapter_start_kwargs_forward_full_rpc_start_shape() {
        let call = call_kw(
            "session.spawn",
            vec![
                ("program", Arg::String("/bin/sh".into())),
                (
                    "args",
                    Arg::List(vec![Arg::String("-lc".into()), Arg::String("cat".into())]),
                ),
                ("cwd", Arg::String("/tmp".into())),
                (
                    "env",
                    Arg::Object(
                        [("NO_COLOR".to_string(), Arg::String("1".into()))]
                            .into_iter()
                            .collect(),
                    ),
                ),
                ("rows", Arg::Int(60)),
                ("cols", Arg::Int(200)),
                ("pixel_width", Arg::Int(1200)),
                ("pixel_height", Arg::Int(800)),
            ],
        );
        let mut params = Map::new();
        insert_adapter_start_kwargs(&mut params, &call).unwrap();
        assert_eq!(params["program"], "/bin/sh");
        assert_eq!(params["args"], json!(["-lc", "cat"]));
        assert_eq!(params["cwd"], "/tmp");
        assert_eq!(params["env"], json!({ "NO_COLOR": "1" }));
        assert_eq!(params["rows"], 60);
        assert_eq!(params["cols"], 200);
        assert_eq!(params["pixel_width"], 1200);
        assert_eq!(params["pixel_height"], 800);
    }

    #[test]
    fn adapter_start_kwargs_reject_wrong_list_map_and_u16_types() {
        let upper_bound = call_kw("session.spawn", vec![("rows", Arg::Int(65_535))]);
        let mut params = Map::new();
        insert_adapter_start_kwargs(&mut params, &upper_bound).unwrap();
        assert_eq!(params["rows"], 65_535);

        let bad_args = call_kw("session.spawn", vec![("args", Arg::String("-lc".into()))]);
        let error = insert_adapter_start_kwargs(&mut Map::new(), &bad_args).unwrap_err();
        assert!(error.to_string().contains("string list"), "{error}");

        let bad_env = call_kw(
            "session.spawn",
            vec![(
                "env",
                Arg::Object(
                    [("NO_COLOR".to_string(), Arg::Bool(true))]
                        .into_iter()
                        .collect(),
                ),
            )],
        );
        let error = insert_adapter_start_kwargs(&mut Map::new(), &bad_env).unwrap_err();
        assert!(error.to_string().contains("string values"), "{error}");

        let bad_rows = call_kw("session.spawn", vec![("rows", Arg::Int(-1))]);
        let error = insert_adapter_start_kwargs(&mut Map::new(), &bad_rows).unwrap_err();
        assert!(error.to_string().contains("0..65535"), "{error}");

        let too_large_rows = call_kw("session.spawn", vec![("rows", Arg::Int(65_536))]);
        let error = insert_adapter_start_kwargs(&mut Map::new(), &too_large_rows).unwrap_err();
        assert!(error.to_string().contains("0..65535"), "{error}");
    }

    #[test]
    fn arg_to_json_and_kwargs_to_json_roundtrip_every_variant() {
        let kwargs: BTreeMap<String, Arg> = [
            ("s".to_string(), Arg::String("hi".into())),
            ("re".to_string(), Arg::Regex("^x$".into())),
            ("d".to_string(), Arg::Duration(Duration::from_millis(750))),
            ("i".to_string(), Arg::Int(-3)),
            ("b".to_string(), Arg::Bool(false)),
            ("n".to_string(), Arg::Null),
            (
                "list".to_string(),
                Arg::List(vec![Arg::String("a".into()), Arg::Int(2)]),
            ),
            (
                "obj".to_string(),
                Arg::Object(
                    [("k".to_string(), Arg::String("v".into()))]
                        .into_iter()
                        .collect(),
                ),
            ),
        ]
        .into_iter()
        .collect();
        let value = kwargs_to_json(&kwargs);
        assert_eq!(value["s"], "hi");
        assert_eq!(value["re"], "^x$");
        assert_eq!(value["d"], 750); // Duration is serialised as ms
        assert_eq!(value["i"], -3);
        assert_eq!(value["b"], false);
        assert!(value["n"].is_null());
        assert_eq!(value["list"], json!(["a", 2]));
        assert_eq!(value["obj"], json!({ "k": "v" }));

        // Nested call collapses to its path string so that arbitrary
        // user-supplied `intent` payloads stay JSON-safe even if
        // they contain a stray DSL call.
        let nested = Arg::Call(call("foo.bar", vec![Arg::Int(1)]));
        let value = arg_to_json(&nested);
        assert_eq!(value, json!("foo.bar(...)"));
    }

    #[test]
    fn parse_session_attach_with_string_and_default() {
        let cmd = parse(r#"session.attach("e7")"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.path, vec!["session", "attach"]);
        assert_eq!(call.positional, vec![Arg::String("e7".into())]);

        // `session.attach()` with no positional arg means "all".
        let cmd = parse("session.attach()").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert!(call.positional.is_empty());
    }

    #[test]
    fn parse_raw_string_preserves_backslashes() {
        // Raw strings are the canonical way to write regexes in this DSL;
        // they must not interpret `\n` as newline.
        let cmd = parse(r#"wait(matches(r"a\n\t"))"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        let Arg::Call(inner) = &call.positional[0] else {
            panic!()
        };
        assert_eq!(inner.positional, vec![Arg::Regex(r"a\n\t".into())]);
    }

    #[test]
    fn parse_negative_integer_kwarg() {
        let cmd = parse("send.intent(\"x\", count=-5)").unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.kwargs.get("count"), Some(&Arg::Int(-5)));
    }

    #[test]
    fn dispatch_unknown_command_returns_help_hint() {
        // Pure error-path branch: the dispatcher's catch-all reports a
        // friendly "try :help" hint rather than panicking on an unknown
        // DSL path. No RPC client is needed because the error fires
        // before any network call.
        let (client, _server, mut ctx) = in_process_client();
        let error = dispatch(
            parse("session.fly()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("session.fly"), "{text}");
        assert!(text.contains(":help"), "{text}");
    }

    #[test]
    fn dispatch_session_attach_with_non_string_arg_errors() {
        // The `session.attach(...)` DSL form takes a string id or
        // `"all"`. A duration or int is a parse-time error message,
        // exercised here to lock the wording.
        let (client, _server, mut ctx) = in_process_client();
        let error = dispatch(
            parse("session.attach(42)").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("session.attach"), "{error}");
    }

    #[test]
    fn dispatch_meta_focus_unknown_id_errors() {
        let (client, _server, mut ctx) = in_process_client();
        ctx.upsert_adapter("e1", "claude-code");
        let error = dispatch(
            parse(":focus e9").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("e9"), "{error}");
    }

    #[test]
    fn dispatch_meta_focus_sets_ctx_focus() {
        let (client, _server, mut ctx) = in_process_client();
        ctx.upsert_adapter("e1", "claude-code");
        ctx.upsert_adapter("e2", "claude-code");
        let outcome = dispatch(
            parse(":focus e2").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Line(line) = outcome else {
            panic!()
        };
        assert!(line.contains("e2"), "{line}");
        assert_eq!(ctx.focus.as_deref(), Some("e2"));
    }

    #[test]
    fn dispatch_session_list_renders_empty_and_populated() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse("session.list()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Line(line) = outcome else {
            panic!()
        };
        assert!(line.contains("no adapters"), "{line}");

        ctx.upsert_adapter("e1", "claude-code");
        let outcome = dispatch(
            parse("session.list()").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Line(line) = outcome else {
            panic!()
        };
        assert!(line.contains("e1"), "{line}");
        assert!(line.contains("claude-code"), "{line}");
    }

    #[test]
    fn dispatch_meta_tabs_empty_renders_placeholder() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse(":tabs").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::Line(line) = outcome else {
            panic!()
        };
        assert!(line.contains("no adapters"), "{line}");
    }

    #[test]
    fn parse_string_handles_every_named_escape() {
        // Each `\X` escape lives in its own match arm; if a future
        // contributor drops one, the parser silently lets the literal
        // character through rather than producing the intended byte.
        let cmd = parse(r#"send.text("\t\r\\\"\0")"#).unwrap();
        let Cmd::Dsl(call) = cmd else { panic!() };
        assert_eq!(call.positional, vec![Arg::String("\t\r\\\"\0".into())]);
    }

    #[test]
    fn parse_unknown_string_escape_errors() {
        let error = parse(r#"send.text("hi\q there")"#).unwrap_err();
        assert!(error.to_string().contains("\\q"), "{error}");
    }

    #[test]
    fn parse_unterminated_raw_string_errors() {
        let error = parse(r#"wait(matches(r"^x"#).unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("unterminated"),
            "{error}"
        );
    }

    #[test]
    fn parse_unexpected_character_errors() {
        // `@` is not a legal token start; the lexer's catch-all branch
        // must surface a friendly message instead of an opaque panic.
        let error = parse("session.spawn(@)").unwrap_err();
        assert!(error.to_string().contains('@'), "{error}");
    }

    #[test]
    fn parse_meta_rpc_invalid_json_errors() {
        let error = parse(r#":rpc adapter.send {not-json}"#).unwrap_err();
        assert!(error.to_string().to_lowercase().contains("json"), "{error}");
    }

    #[test]
    fn parse_dotted_path_missing_identifier_errors() {
        // `foo.` with nothing after the dot — the path parser bails on
        // the missing ident rather than emitting an empty segment.
        let error = parse("foo.()").unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("identifier")
                || error.to_string().contains("RParen"),
            "{error}"
        );
    }

    #[test]
    fn parse_arg_list_with_missing_close_paren_errors() {
        let error = parse("session.spawn(\"x\"").unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn dispatch_help_returns_help_outcome() {
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse(":help").unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .unwrap();
        let CmdOutcome::ShowHelp(text) = outcome else {
            panic!("expected ShowHelp, got {outcome:?}")
        };
        assert!(text.contains("plugins()"), "help missing top-line cmds");
        assert!(text.contains("send.key"), "help missing send.key entry");
        assert!(
            text.contains("resume and close prior id"),
            "help missing session.resume description"
        );
        assert!(text.contains("alias: prior"), "help missing prior alias");
        assert!(text.contains("plugins.describe"), "help missing describe");
        assert!(text.contains("turn(\"intent\""), "help missing turn() form");
        assert!(
            text.contains(":notifications on|off [adapters="),
            "help missing notifications filter syntax"
        );
    }

    #[test]
    fn dispatch_cancel_wait_calls_adapter_cancel_wait() {
        // `cancel_wait("some-id")` must serialize as
        // `adapter.cancel_wait { wait_id: "some-id" }`. An unknown
        // wait_id returns `cancelled: false` per the idempotency
        // contract (no server-side state to verify, so a fresh in-
        // process server is enough to exercise the wire shape).
        let (client, _server, mut _ctx) = in_process_client();
        let outcome = dispatch(
            parse(r#"cancel_wait("never-was-registered")"#).unwrap(),
            &client,
            &mut _ctx,
            Duration::from_secs(2),
        )
        .expect("cancel_wait dispatch");
        let CmdOutcome::Json(value) = outcome else {
            panic!("expected json outcome, got {outcome:?}")
        };
        assert_eq!(value["cancelled"], false);
    }

    #[test]
    fn dispatch_plugins_describe_routes_to_plugin_describe() {
        // `plugins.describe("claude-code")` must call `plugin.describe`
        // with `{ plugin: "claude-code" }`. The built-in claude-code
        // plugin exports a `describe()` function, so the server should
        // return its intents/wait_matchers/states catalog verbatim.
        let (client, _server, mut ctx) = in_process_client();
        let outcome = dispatch(
            parse(r#"plugins.describe("claude-code")"#).unwrap(),
            &client,
            &mut ctx,
            Duration::from_secs(5),
        )
        .expect("plugins.describe dispatch");
        let CmdOutcome::Json(value) = outcome else {
            panic!("expected json outcome, got {outcome:?}")
        };
        assert_eq!(value["plugin"], "claude-code");
        let intents = value["intents"]
            .as_array()
            .expect("intents must be an array");
        let names: Vec<&str> = intents
            .iter()
            .filter_map(|entry| entry.get("name").and_then(|n| n.as_str()))
            .collect();
        // The plugin's own describe() must surface `send_prompt`.
        assert!(
            names.contains(&"send_prompt"),
            "intents missing send_prompt: {names:?}"
        );
    }

    #[test]
    fn turn_command_builds_send_and_wait_blocks() {
        // The DSL `turn("send_prompt", prompt="hi", timeout=5s)` must
        // serialize to `{ adapter, send: { intent: "send_prompt",
        // params: { prompt: "hi" } }, wait: { timeout_ms: 5000 } }`.
        // Reserved kwargs (`wait`, `wait_intent`, `timeout`) MUST NOT
        // leak into `send.params`.
        let call = match parse(r#"turn("send_prompt", prompt="hi", timeout=5s)"#).unwrap() {
            Cmd::Dsl(call) => call,
            other => panic!("expected DSL call, got {other:?}"),
        };
        let intent = expect_one_string(&call, "turn").unwrap();
        assert_eq!(intent, "send_prompt");
        // send_kwargs should only contain `prompt`, not the reserved
        // wait kwargs.
        let mut send_kwargs = BTreeMap::new();
        for (key, value) in &call.kwargs {
            if matches!(key.as_str(), "wait" | "wait_intent" | "timeout") {
                continue;
            }
            send_kwargs.insert(key.clone(), value.clone());
        }
        let send_params = kwargs_to_json(&send_kwargs);
        assert_eq!(send_params, json!({ "prompt": "hi" }));
        assert_eq!(
            duration_kwarg(&call, "timeout").unwrap(),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn turn_command_resolves_wait_matcher_shortcut() {
        // `turn(..., wait=matches(r"…"))` must walk through
        // `matcher_to_params` so the wire shape matches what the
        // bare `wait()` form would produce.
        let call = match parse(r#"turn("send_prompt", wait=matches(r"completed"))"#).unwrap() {
            Cmd::Dsl(call) => call,
            other => panic!("expected DSL call, got {other:?}"),
        };
        let wait_arg = call.kwargs.get("wait").cloned().expect("wait kwarg");
        let (intent, params) = matcher_to_params(wait_arg).unwrap();
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params, json!({ "pattern": "completed" }));
    }
}
