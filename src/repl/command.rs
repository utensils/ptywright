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
    Call(DslCall),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetaCmd {
    Help,
    Quit,
    Tabs,
    Focus(String),
    Notifications(bool),
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

/// Outcome of a dispatched command — what the TUI's history pane renders.
#[derive(Debug, Clone)]
pub enum CmdOutcome {
    /// Human-readable line (the dispatched command produced no result we
    /// want to dump into the history pane, e.g. `:tabs` or `:focus`).
    Line(String),
    /// JSON result from a JSON-RPC call. The TUI pretty-prints it.
    Json(Value),
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
    Comma,
    Eq,
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
            ',' => {
                self.advance();
                Tok::Comma
            }
            '=' => {
                self.advance();
                Tok::Eq
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
        "notifications" => {
            let enabled = match tail {
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                other => {
                    return Err(Error::Rpc(format!(
                        ":notifications expects on|off, got `{other}`",
                    )));
                }
            };
            MetaCmd::Notifications(enabled)
        }
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
            Some(tok) => Err(Error::Rpc(format!("expected argument, found {tok:?}"))),
            None => Err(Error::Rpc("expected argument".to_string())),
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
        MetaCmd::Notifications(enabled) => {
            let result = client.call(
                "server.set_notifications",
                json!({ "enabled": enabled }),
                timeout,
            )?;
            // Pause the client's heartbeat in lockstep — otherwise it
            // would re-assert `set_notifications {enabled: true}` every
            // interval and silently undo `:notifications off`.
            client.set_heartbeat_enabled(enabled);
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
        "session.spawn" => session_spawn(call, client, ctx, timeout),
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
            send_named_intent(client, ctx, "send_prompt", json!({ "text": text }), timeout)
        }
        "send.key" => {
            let key = expect_one_string(&call, "send.key")?;
            send_named_intent(client, ctx, "key", json!({ "key": key }), timeout)
        }
        "wait" => wait_command(call, client, ctx, timeout),
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
            Ok(CmdOutcome::Json(result))
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
    if let Some(program) = string_kwarg(&call, "program") {
        params.insert("program".to_string(), Value::String(program));
    }
    if let Some(rows) = int_kwarg(&call, "rows") {
        params.insert("rows".to_string(), Value::from(rows));
    }
    if let Some(cols) = int_kwarg(&call, "cols") {
        params.insert("cols".to_string(), Value::from(cols));
    }
    let result = client.call("adapter.start", Value::Object(params), timeout)?;
    if let Some(adapter) = result.get("adapter").and_then(Value::as_str) {
        let response_plugin = result
            .get("plugin")
            .and_then(Value::as_str)
            .unwrap_or(&plugin);
        ctx.upsert_adapter(adapter, response_plugin);
        ctx.focus = Some(adapter.to_string());
    }
    Ok(CmdOutcome::Json(result))
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
    Ok(CmdOutcome::Json(result))
}

fn send_intent(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let intent = expect_one_string(&call, "send.intent")?;
    let params = kwargs_to_json(&call.kwargs);
    send_named_intent(client, ctx, &intent, params, timeout)
}

fn send_named_intent(
    client: &RpcClient,
    ctx: &ReplCtx,
    intent: &str,
    params: Value,
    timeout: Duration,
) -> Result<CmdOutcome> {
    let adapter = focus_or_err(ctx)?.to_string();
    let result = client.call(
        "adapter.send",
        json!({
            "adapter": adapter,
            "intent": intent,
            "params": params,
        }),
        timeout,
    )?;
    Ok(CmdOutcome::Json(result))
}

fn wait_command(
    call: DslCall,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<CmdOutcome> {
    // `wait(matcher, timeout=2s)` — the first positional arg is either a
    // nested call (matches/screen_stable/...) or a regex/duration shortcut.
    let matcher_arg = call.positional.first().cloned().ok_or_else(|| {
        Error::Rpc(
            "wait() expects a matcher: wait(matches(r\"…\")) or wait(screen_stable(250ms))"
                .to_string(),
        )
    })?;
    let timeout_arg = duration_kwarg(&call, "timeout")?;
    let (intent, params) = matcher_to_params(matcher_arg)?;
    wait_with_params(client, ctx, &intent, params, timeout_arg, timeout)
}

fn wait_with_params(
    client: &RpcClient,
    ctx: &mut ReplCtx,
    intent: &str,
    matcher_params: Value,
    timeout_override: Option<Duration>,
    fallback_timeout: Duration,
) -> Result<CmdOutcome> {
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
    let result = client.call("adapter.wait", Value::Object(req), fallback_timeout)?;
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

fn int_kwarg(call: &DslCall, name: &str) -> Option<i64> {
    match call.kwargs.get(name)? {
        Arg::Int(n) => Some(*n),
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
        Arg::Call(call) => Value::String(format!("{}(...)", call.path.join("."))),
    }
}

pub fn help_text() -> &'static str {
    "ptywright repl commands:\n\
     \n\
     Sessions:\n\
       plugins()                       list built-in plugins\n\
       session.spawn(\"name\")         spawn an adapter for the named plugin\n\
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
       wait(matches(r\"…\"))            wait for output to match\n\
       wait(screen_stable(250ms))      wait for the screen to settle\n\
       transcript.snapshot(redact=true)\n\
       screen.snapshot()                render the focused PTY inline\n\
       view()                           alias for screen.snapshot()\n\
       inspect()                        diagnostic dump\n\
     \n\
     Meta:\n\
       :tabs       :focus <id>   :live   :attach <id|all>\n\
       :notifications on|off     :rpc <method> {json}\n\
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
            Cmd::Meta(MetaCmd::Notifications(true))
        );
        assert_eq!(
            parse(":notifications off").unwrap(),
            Cmd::Meta(MetaCmd::Notifications(false))
        );
    }

    #[test]
    fn parse_meta_notifications_rejects_garbage() {
        let error = parse(":notifications maybe").unwrap_err();
        assert!(error.to_string().contains("on|off"), "{error}");
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
        let CmdOutcome::Json(value) = outcome else {
            panic!()
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
        let CmdOutcome::Json(value) = spawn else {
            panic!("expected json outcome from spawn")
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
}
