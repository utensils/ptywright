use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::action::Action;
use crate::adapters::{ClaudeCodeAdapter, ClaudeCodeConfig};
use crate::error::{Error, Result};
use crate::matcher::Matcher;
use crate::plugin::{PluginHostCapabilities, PluginManifest};
use crate::redaction::RedactionPolicy;
use crate::session::{Session, SessionConfig};
use crate::target::{Target, TerminalSize};
use crate::transcript::TranscriptFileConfig;
use crate::{NAME, VERSION};

const JSONRPC_VERSION: &str = "2.0";

/// Shared JSON-RPC session registry used by multi-client local IPC transports.
#[derive(Clone, Default)]
pub struct RpcServerState {
    inner: Arc<Mutex<RpcSharedState>>,
}

struct RpcSharedState {
    sessions: HashMap<String, Arc<Session>>,
    next_session: u64,
}

impl Default for RpcSharedState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            next_session: 1,
        }
    }
}

/// Stateful JSON-RPC handler for one client connection.
pub struct RpcServer {
    shared: RpcServerState,
    claude_adapters: HashMap<String, ClaudeCodeAdapter>,
    next_claude_adapter: u64,
    notifications_enabled: bool,
    last_notified_sequences: HashMap<String, u64>,
    notified_exits: HashSet<String>,
}

#[derive(Debug, Deserialize)]
struct Request {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CreateParams {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    rows: Option<u16>,
    cols: Option<u16>,
    pixel_width: Option<u16>,
    pixel_height: Option<u16>,
    transcript_max_chars: Option<usize>,
    raw_transcript_path: Option<PathBuf>,
    raw_transcript_append: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SessionParams {
    session: String,
}

#[derive(Debug, Deserialize)]
struct SessionReadParams {
    session: String,
    /// Whether to redact sensitive-looking output fields. Defaults to true.
    redact: Option<bool>,
    /// Optional caller-supplied redaction additions/replacement for this read.
    redaction: Option<RedactionPolicy>,
}

#[derive(Debug, Deserialize)]
struct ResizeParams {
    session: String,
    rows: u16,
    cols: u16,
    pixel_width: Option<u16>,
    pixel_height: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct InputParams {
    session: String,
    action: Action,
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    session: String,
    matcher: Matcher,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ClaudeStartParams {
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    rows: Option<u16>,
    cols: Option<u16>,
    pixel_width: Option<u16>,
    pixel_height: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct ClaudeParams {
    claude: String,
}

#[derive(Debug, Deserialize)]
struct ClaudePromptParams {
    claude: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeWaitParams {
    claude: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestParams {
    manifest: PluginManifest,
}

#[derive(Debug, Deserialize)]
struct NotificationsParams {
    enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    Timeout,
    SessionClosed,
}

impl RpcErrorCode {
    const fn code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::Timeout => -32001,
            Self::SessionClosed => -32002,
        }
    }
}

impl RpcServerState {
    /// Create an empty shared JSON-RPC server state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RpcServer {
    /// Create an empty JSON-RPC server with private state.
    #[must_use]
    pub fn new() -> Self {
        Self::with_state(RpcServerState::new())
    }

    /// Create a JSON-RPC handler for one client over shared server state.
    #[must_use]
    pub fn with_state(shared: RpcServerState) -> Self {
        Self {
            shared,
            claude_adapters: HashMap::new(),
            next_claude_adapter: 1,
            notifications_enabled: false,
            last_notified_sequences: HashMap::new(),
            notified_exits: HashSet::new(),
        }
    }

    /// Handle one NDJSON-framed JSON-RPC message.
    ///
    /// Returns `Ok(None)` for JSON-RPC notifications, which have no `id` and
    /// therefore do not receive responses.
    pub fn handle_line(&mut self, line: &str) -> Result<Option<String>> {
        let parsed = serde_json::from_str::<Request>(line);
        let request = match parsed {
            Ok(request) => request,
            Err(error) => {
                return serialize_response(error_response(
                    None,
                    RpcErrorCode::ParseError,
                    format!("parse error: {error}"),
                ))
                .map(Some);
            }
        };

        let id = request.id.clone();
        let response_required = id.is_some();
        let response = match self.handle_request(request) {
            Ok(result) => success_response(id, result),
            Err((code, message)) => error_response(id, code, message),
        };

        if response_required {
            serialize_response(response).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Handle one message and return the response plus any enabled notifications.
    pub fn handle_line_messages(&mut self, line: &str) -> Result<Vec<String>> {
        let mut messages = Vec::new();
        if let Some(response) = self.handle_line(line)? {
            messages.push(response);
        }
        if self.notifications_enabled {
            messages.extend(self.poll_notifications()?);
        }
        Ok(messages)
    }

    fn poll_notifications(&mut self) -> Result<Vec<String>> {
        let mut messages = Vec::new();
        let mut sessions = self
            .shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), Arc::clone(session)))
            .collect::<Vec<_>>();
        sessions.sort_by(|(left, _), (right, _)| left.cmp(right));

        for (id, session) in sessions {
            let sequence = session.sequence();
            if self.last_notified_sequences.get(&id).copied() != Some(sequence) {
                self.last_notified_sequences.insert(id.clone(), sequence);
                messages.push(serialize_response(json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "method": "session.changed",
                    "params": {
                        "session": id,
                        "sequence": sequence,
                    },
                }))?);
            }
            if session.is_finished() && self.notified_exits.insert(id.clone()) {
                messages.push(serialize_response(json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "method": "session.exited",
                    "params": {
                        "session": id,
                        "sequence": sequence,
                    },
                }))?);
            }
        }

        Ok(messages)
    }

    fn handle_request(
        &mut self,
        request: Request,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        if request.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
            return Err((
                RpcErrorCode::InvalidRequest,
                "jsonrpc must be \"2.0\"".to_string(),
            ));
        }
        let method = request.method.ok_or_else(|| {
            (
                RpcErrorCode::InvalidRequest,
                "method is required".to_string(),
            )
        })?;

        match method.as_str() {
            "server.capabilities" => Ok(json!({
                "name": NAME,
                "version": VERSION,
                "framing": ["ndjson", "lsp"],
                "methods": [
                    "server.capabilities",
                    "server.set_notifications",
                    "session.create",
                    "session.list",
                    "session.close",
                    "session.kill",
                    "session.resize",
                    "session.input",
                    "session.snapshot",
                    "session.transcript",
                    "session.wait",
                    "claude.start",
                    "claude.send_prompt",
                    "claude.wait_turn",
                    "claude.approve",
                    "claude.deny",
                    "claude.cancel",
                    "claude.state",
                    "plugin.capabilities",
                    "plugin.validate_manifest"
                ],
                "notifications": ["session.changed", "session.exited"]
            })),
            "server.set_notifications" => self.server_set_notifications(request.params),
            "session.create" => self.session_create(request.params),
            "session.list" => Ok(self.session_list()),
            "session.close" => self.session_close(request.params),
            "session.kill" => self.session_kill(request.params),
            "session.resize" => self.session_resize(request.params),
            "session.input" => self.session_input(request.params),
            "session.snapshot" => self.session_snapshot(request.params),
            "session.transcript" => self.session_transcript(request.params),
            "session.wait" => self.session_wait(request.params),
            "claude.start" => self.claude_start(request.params),
            "claude.send_prompt" => self.claude_send_prompt(request.params),
            "claude.wait_turn" => self.claude_wait_turn(request.params),
            "claude.approve" => self.claude_approve(request.params),
            "claude.deny" => self.claude_deny(request.params),
            "claude.cancel" => self.claude_cancel(request.params),
            "claude.state" => self.claude_state(request.params),
            "plugin.capabilities" => Ok(json!(PluginHostCapabilities::current())),
            "plugin.validate_manifest" => self.plugin_validate_manifest(request.params),
            _ => Err((
                RpcErrorCode::MethodNotFound,
                format!("unknown method: {method}"),
            )),
        }
    }

    fn server_set_notifications(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: NotificationsParams = parse_params(params)?;
        self.notifications_enabled = params.enabled;
        Ok(json!({ "enabled": self.notifications_enabled }))
    }

    fn session_create(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: CreateParams = parse_params(params)?;
        let size = TerminalSize {
            rows: params.rows.unwrap_or(24),
            cols: params.cols.unwrap_or(80),
            pixel_width: params.pixel_width.unwrap_or(0),
            pixel_height: params.pixel_height.unwrap_or(0),
        };
        let mut target = Target::new(params.program).args(params.args).size(size);
        target.cwd = params.cwd;
        target.env = params.env;
        let mut config = SessionConfig::new(target);
        if let Some(max_chars) = params.transcript_max_chars {
            config.transcript.max_chars = max_chars;
        }
        if params.raw_transcript_append == Some(true) && params.raw_transcript_path.is_none() {
            return Err((
                RpcErrorCode::InvalidParams,
                "raw_transcript_append requires raw_transcript_path".to_string(),
            ));
        }
        if let Some(path) = params.raw_transcript_path {
            config.transcript.raw_file = Some(
                TranscriptFileConfig::new(path)
                    .append(params.raw_transcript_append.unwrap_or(false)),
            );
        }
        let session = Arc::new(Session::spawn(config).map_err(rpc_error_from_error)?);
        let id = self.allocate_session_id();
        self.shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .insert(id.clone(), session);
        Ok(json!({ "session": id }))
    }

    fn session_list(&self) -> Value {
        let mut sessions = self
            .shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort();
        json!({ "sessions": sessions })
    }

    fn session_close(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionParams = parse_params(params)?;
        let session = self
            .shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .remove(&params.session);
        if let Some(session) = session {
            let _ = session.kill();
            self.last_notified_sequences.remove(&params.session);
            self.notified_exits.remove(&params.session);
            Ok(json!({ "closed": true }))
        } else {
            Err((
                RpcErrorCode::InvalidParams,
                format!("unknown session: {}", params.session),
            ))
        }
    }

    fn session_kill(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionParams = parse_params(params)?;
        let session = self.session(&params.session)?;
        session.kill().map_err(rpc_error_from_error)?;
        Ok(json!({ "killed": true }))
    }

    fn session_resize(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ResizeParams = parse_params(params)?;
        let size = TerminalSize {
            rows: params.rows,
            cols: params.cols,
            pixel_width: params.pixel_width.unwrap_or(0),
            pixel_height: params.pixel_height.unwrap_or(0),
        };
        self.session(&params.session)?
            .resize(size)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "resized": true }))
    }

    fn session_input(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: InputParams = parse_params(params)?;
        self.session(&params.session)?
            .send(params.action)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "sent": true }))
    }

    fn session_snapshot(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionReadParams = parse_params(params)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let mut snapshot = self.session(&params.session)?.snapshot();
        if let Some(policy) = policy {
            snapshot = snapshot.redacted(&policy);
        }
        serde_json::to_value(snapshot)
            .map_err(|error| (RpcErrorCode::InternalError, error.to_string()))
    }

    fn session_transcript(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionReadParams = parse_params(params)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let session = self.session(&params.session)?;
        let text = if let Some(policy) = policy {
            session.redacted_transcript(&policy)
        } else {
            session.transcript()
        };
        Ok(json!({ "text": text }))
    }

    fn session_wait(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: WaitParams = parse_params(params)?;
        let result = self
            .session(&params.session)?
            .wait_for(
                &params.matcher,
                Duration::from_millis(params.timeout_ms.unwrap_or(30_000)),
            )
            .map_err(rpc_error_from_error)?;
        Ok(json!({
            "matched": result.matched,
            "sequence": result.sequence,
            "elapsed_ms": result.elapsed.as_millis(),
            "snapshot": result.snapshot,
            "transcript_tail": result.transcript_tail,
        }))
    }

    fn claude_start(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeStartParams = parse_params(params)?;
        let config = ClaudeCodeConfig {
            program: params.program.unwrap_or_else(|| "claude".to_string()),
            args: params.args,
            cwd: params.cwd,
            env: params.env,
            size: TerminalSize {
                rows: params.rows.unwrap_or(40),
                cols: params.cols.unwrap_or(120),
                pixel_width: params.pixel_width.unwrap_or(0),
                pixel_height: params.pixel_height.unwrap_or(0),
            },
        };
        let adapter = ClaudeCodeAdapter::start(config).map_err(rpc_error_from_error)?;
        let state = adapter.state();
        let id = self.allocate_claude_adapter_id();
        self.claude_adapters.insert(id.clone(), adapter);
        Ok(json!({ "claude": id, "state": state }))
    }

    fn claude_send_prompt(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudePromptParams = parse_params(params)?;
        let state = self
            .claude_adapter_mut(&params.claude)?
            .send_prompt(params.prompt)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    fn claude_wait_turn(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeWaitParams = parse_params(params)?;
        let state = self
            .claude_adapter(&params.claude)?
            .wait_turn(Duration::from_millis(params.timeout_ms.unwrap_or(120_000)))
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    fn claude_approve(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeParams = parse_params(params)?;
        self.claude_adapter(&params.claude)?
            .approve()
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "approved": true }))
    }

    fn claude_deny(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeParams = parse_params(params)?;
        self.claude_adapter(&params.claude)?
            .deny()
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "denied": true }))
    }

    fn claude_cancel(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeParams = parse_params(params)?;
        let state = self
            .claude_adapter_mut(&params.claude)?
            .cancel()
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    fn claude_state(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: ClaudeParams = parse_params(params)?;
        Ok(json!({ "state": self.claude_adapter(&params.claude)?.state() }))
    }

    fn plugin_validate_manifest(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: PluginManifestParams = parse_params(params)?;
        params
            .manifest
            .validate()
            .map_err(|error| (RpcErrorCode::InvalidParams, error.to_string()))?;
        Ok(json!({ "valid": true }))
    }

    fn claude_adapter(
        &self,
        id: &str,
    ) -> std::result::Result<&ClaudeCodeAdapter, (RpcErrorCode, String)> {
        self.claude_adapters.get(id).ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown Claude Code adapter: {id}"),
            )
        })
    }

    fn claude_adapter_mut(
        &mut self,
        id: &str,
    ) -> std::result::Result<&mut ClaudeCodeAdapter, (RpcErrorCode, String)> {
        self.claude_adapters.get_mut(id).ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown Claude Code adapter: {id}"),
            )
        })
    }

    fn session(&self, id: &str) -> std::result::Result<Arc<Session>, (RpcErrorCode, String)> {
        self.shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| {
                (
                    RpcErrorCode::InvalidParams,
                    format!("unknown session: {id}"),
                )
            })
    }

    fn allocate_session_id(&mut self) -> String {
        let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let id = format!("s{}", shared.next_session);
        shared.next_session += 1;
        id
    }

    fn allocate_claude_adapter_id(&mut self) -> String {
        let id = format!("c{}", self.next_claude_adapter);
        self.next_claude_adapter += 1;
        id
    }
}

impl Default for RpcServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Run an NDJSON-framed JSON-RPC server over arbitrary input/output streams.
pub fn serve_ndjson(input: impl Read, output: impl Write) -> Result<()> {
    let mut server = RpcServer::new();
    serve_ndjson_with_server(input, output, &mut server)
}

/// Run an NDJSON-framed JSON-RPC server over shared state.
pub fn serve_ndjson_with_state(
    input: impl Read,
    output: impl Write,
    state: RpcServerState,
) -> Result<()> {
    let mut server = RpcServer::with_state(state);
    serve_ndjson_with_server(input, output, &mut server)
}

fn serve_ndjson_with_server(
    input: impl Read,
    mut output: impl Write,
    server: &mut RpcServer,
) -> Result<()> {
    let reader = BufReader::new(input);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        for message in server.handle_line_messages(&line)? {
            output.write_all(message.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Run an LSP-style `Content-Length` framed JSON-RPC server over arbitrary streams.
pub fn serve_lsp(input: impl Read, output: impl Write) -> Result<()> {
    let mut server = RpcServer::new();
    serve_lsp_with_server(input, output, &mut server)
}

/// Run an LSP-style `Content-Length` framed JSON-RPC server over shared state.
pub fn serve_lsp_with_state(
    input: impl Read,
    output: impl Write,
    state: RpcServerState,
) -> Result<()> {
    let mut server = RpcServer::with_state(state);
    serve_lsp_with_server(input, output, &mut server)
}

fn serve_lsp_with_server(
    input: impl Read,
    mut output: impl Write,
    server: &mut RpcServer,
) -> Result<()> {
    let mut reader = BufReader::new(input);
    while let Some(payload) = read_lsp_payload(&mut reader)? {
        if payload.trim().is_empty() {
            continue;
        }
        for message in server.handle_line_messages(&payload)? {
            write_lsp_payload(&mut output, &message)?;
        }
    }
    Ok(())
}

fn read_lsp_payload(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut content_length = None;
    let mut saw_header = false;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            if saw_header {
                return Err(Error::Rpc("unexpected EOF in LSP headers".to_string()));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        saw_header = true;
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            content_length =
                Some(value.trim().parse::<usize>().map_err(|error| {
                    Error::Rpc(format!("invalid Content-Length header: {error}"))
                })?);
        }
    }

    let length =
        content_length.ok_or_else(|| Error::Rpc("missing Content-Length header".to_string()))?;
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    String::from_utf8(payload)
        .map(Some)
        .map_err(|error| Error::Rpc(format!("LSP payload is not valid UTF-8: {error}")))
}

fn write_lsp_payload(output: &mut impl Write, payload: &str) -> Result<()> {
    write!(output, "Content-Length: {}\r\n\r\n", payload.len())?;
    output.write_all(payload.as_bytes())?;
    output.flush()?;
    Ok(())
}

fn parse_params<T>(params: Option<Value>) -> std::result::Result<T, (RpcErrorCode, String)>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| (RpcErrorCode::InvalidParams, error.to_string()))
}

fn redaction_policy_for_read(
    policy: Option<RedactionPolicy>,
) -> std::result::Result<RedactionPolicy, (RpcErrorCode, String)> {
    let mut policy = policy.unwrap_or_default();
    policy.enabled = true;
    policy
        .validate()
        .map_err(|error| (RpcErrorCode::InvalidParams, error))?;
    Ok(policy)
}

fn success_response(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn error_response(id: Option<Value>, code: RpcErrorCode, message: String) -> Value {
    let message = RedactionPolicy::default().redact(&message);
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "error": {
            "code": code.code(),
            "message": message,
        },
    })
}

fn serialize_response(value: Value) -> Result<String> {
    Ok(serde_json::to_string(&value)?)
}

fn rpc_error_from_error(error: Error) -> (RpcErrorCode, String) {
    let code = match error {
        Error::Timeout => RpcErrorCode::Timeout,
        Error::Closed | Error::ReaderEnded => RpcErrorCode::SessionClosed,
        Error::Pty(_)
        | Error::Io(_)
        | Error::Json(_)
        | Error::Lua(_)
        | Error::Rpc(_)
        | Error::Config(_) => RpcErrorCode::InternalError,
    };
    (code, error.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn handle(server: &mut RpcServer, line: &str) -> Value {
        let response = server
            .handle_line(line)
            .expect("handle line")
            .expect("response");
        serde_json::from_str(&response).expect("json response")
    }

    #[test]
    fn capabilities_returns_protocol_shape() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}"#,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        let framing = response["result"]["framing"].as_array().unwrap();
        assert!(framing.contains(&json!("ndjson")));
        assert!(framing.contains(&json!("lsp")));
        let methods = response["result"]["methods"].as_array().unwrap();
        assert!(methods.contains(&json!("server.set_notifications")));
        assert!(methods.contains(&json!("session.create")));
        assert!(methods.contains(&json!("claude.start")));
        assert!(methods.contains(&json!("plugin.validate_manifest")));
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let mut server = RpcServer::new();
        let response = handle(&mut server, "{");

        assert_eq!(response["error"]["code"], -32700);
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn notification_has_no_response() {
        let mut server = RpcServer::new();
        let response = server
            .handle_line(r#"{"jsonrpc":"2.0","method":"server.capabilities"}"#)
            .expect("handle line");

        assert!(response.is_none());
    }

    #[test]
    fn notifications_are_explicitly_enabled() {
        let mut server = RpcServer::new();
        let messages = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
            )
            .expect("handle line");

        assert_eq!(messages.len(), 1);
        let response: Value = serde_json::from_str(&messages[0]).expect("json response");
        assert_eq!(response["result"]["enabled"], true);
    }

    #[test]
    #[cfg(unix)]
    fn shared_state_shares_sessions_across_connection_handlers() {
        let state = RpcServerState::new();
        let mut first = RpcServer::with_state(state.clone());
        let mut second = RpcServer::with_state(state);

        let create = handle(
            &mut first,
            r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","sleep 1"]}}"#,
        );
        let session = create["result"]["session"].as_str().expect("session id");
        let list = handle(
            &mut second,
            r#"{"jsonrpc":"2.0","id":2,"method":"session.list"}"#,
        );

        assert!(
            list["result"]["sessions"]
                .as_array()
                .unwrap()
                .contains(&json!(session))
        );
        let close = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session.close","params":{{"session":"{session}"}}}}"#
        );
        let _ = handle(&mut second, &close);
    }

    #[test]
    fn notification_subscriptions_are_per_connection_handler() {
        let state = RpcServerState::new();
        let mut first = RpcServer::with_state(state.clone());
        let mut second = RpcServer::with_state(state);

        let first_messages = first
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
            )
            .expect("enable first notifications");
        let second_messages = second
            .handle_line_messages(r#"{"jsonrpc":"2.0","id":2,"method":"session.list"}"#)
            .expect("second list without notifications");

        assert_eq!(first_messages.len(), 1);
        assert_eq!(second_messages.len(), 1);
        let response: Value = serde_json::from_str(&second_messages[0]).expect("json response");
        assert_eq!(response["id"], 2);
    }

    #[test]
    #[cfg(unix)]
    fn notifications_report_session_changes() {
        let mut server = RpcServer::new();
        let _ = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
            )
            .expect("enable notifications");
        let create_messages = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":2,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","printf event"]}}"#,
            )
            .expect("create session");
        let create_response: Value =
            serde_json::from_str(&create_messages[0]).expect("json response");
        let session = create_response["result"]["session"].as_str().unwrap();

        let poll_messages = server
            .handle_line_messages(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"session.wait","params":{{"session":"{session}","matcher":{{"type":"contains_text","value":"event"}},"timeout_ms":5000}}}}"#,
            ))
            .expect("wait for output");

        assert!(
            poll_messages
                .iter()
                .any(|message| message.contains("session.changed")),
            "expected session.changed notification, got: {poll_messages:?}"
        );
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":"x","method":"missing"}"#,
        );

        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], "x");
    }

    #[test]
    fn claude_state_for_unknown_adapter_returns_invalid_params() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":9,"method":"claude.state","params":{"claude":"missing"}}"#,
        );

        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn claude_mutation_methods_for_unknown_adapter_return_invalid_params() {
        // approve/deny/cancel all share the same lookup path; cover each so a
        // future refactor that splits the dispatcher cannot silently regress
        // any one of them.
        for (id, method) in [
            (20, "claude.approve"),
            (21, "claude.deny"),
            (22, "claude.cancel"),
        ] {
            let mut server = RpcServer::new();
            let response = handle(
                &mut server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{"claude":"missing"}}}}"#
                ),
            );

            assert_eq!(
                response["error"]["code"], -32602,
                "{method} should reject unknown adapter with InvalidParams"
            );
            let message = response["error"]["message"].as_str().unwrap_or("");
            assert!(
                message.contains("unknown Claude Code adapter"),
                "{method} error message should name the missing lookup, got `{message}`"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn claude_approve_deny_cancel_dispatch_through_rpc_against_live_session() {
        // Use /bin/sh -lc cat as a long-lived stand-in for `claude`: the Lua
        // adapter's approve/deny plans send byte sequences, and `cat` accepts
        // arbitrary input without exiting until EOF or interrupt. This
        // verifies the JSON-RPC dispatch path end-to-end without depending on
        // a real Claude Code install.
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"claude.start","params":{"program":"/bin/sh","args":["-lc","cat"]}}"#,
        );
        let claude = start["result"]["claude"]
            .as_str()
            .expect("claude adapter id");

        let approve = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"claude.approve","params":{{"claude":"{claude}"}}}}"#
            ),
        );
        assert_eq!(approve["result"]["approved"], true);

        let deny = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"claude.deny","params":{{"claude":"{claude}"}}}}"#
            ),
        );
        assert_eq!(deny["result"]["denied"], true);

        // cancel records the cancelling intent on the adapter and returns the
        // resulting state snapshot. The exact classification depends on what
        // bytes `cat` echoed back — assert the response shape, not the state
        // label, since `cat` is not Claude.
        let cancel = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"claude.cancel","params":{{"claude":"{claude}"}}}}"#
            ),
        );
        assert!(
            cancel["result"]["state"].is_object(),
            "cancel must return a state snapshot object"
        );
        assert!(
            cancel["result"]["state"]["state"].is_string(),
            "state snapshot must include a state label"
        );
        assert!(
            cancel["result"]["state"]["sequence"].is_number(),
            "state snapshot must include a sequence number"
        );
    }

    #[test]
    fn rpc_error_messages_are_redacted() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":11,"method":"token=super-secret-value"}"#,
        );

        assert_eq!(response["error"]["code"], -32601);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("token=[REDACTED]"));
        assert!(!message.contains("super-secret-value"));
    }

    #[test]
    fn plugin_validate_manifest_accepts_valid_manifest() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":10,"method":"plugin.validate_manifest","params":{"manifest":{"name":"demo","kind":"adapter","version":"0.1.0","permissions":["session.spawn","screen.read"]}}}"#,
        );

        assert_eq!(response["result"]["valid"], true);
    }

    #[test]
    #[cfg(unix)]
    fn transcript_read_methods_redact_by_default_and_allow_raw_opt_in() {
        let mut server = RpcServer::new();
        let create = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","printf 'token=super-secret'"]}}"#,
        );
        let session = create["result"]["session"].as_str().unwrap();
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"session.wait","params":{{"session":"{session}","matcher":{{"type":"contains_text","value":"token="}},"timeout_ms":5000}}}}"#,
            ),
        );

        let redacted = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"session.transcript","params":{{"session":"{session}"}}}}"#,
            ),
        );
        let raw = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"session.transcript","params":{{"session":"{session}","redact":false}}}}"#,
            ),
        );

        assert_eq!(redacted["result"]["text"], "token=[REDACTED]");
        assert_eq!(raw["result"]["text"], "token=super-secret");
    }

    #[test]
    #[cfg(unix)]
    fn session_create_can_stream_raw_transcript_to_explicit_file() {
        let path = std::env::temp_dir().join(format!(
            "ptywright-rpc-transcript-{}-{}.log",
            std::process::id(),
            unique_suffix()
        ));
        let mut server = RpcServer::new();
        let path_json = serde_json::to_string(&path).expect("serialize path");
        let create = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"session.create","params":{{"program":"/bin/sh","args":["-lc","printf 'token=super-secret'"],"raw_transcript_path":{path_json}}}}}"#
            ),
        );
        let session = create["result"]["session"].as_str().unwrap();
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"session.wait","params":{{"session":"{session}","matcher":{{"type":"process_exited"}},"timeout_ms":5000}}}}"#,
            ),
        );

        let bytes = std::fs::read(&path).expect("read raw transcript");
        assert_eq!(bytes, b"token=super-secret");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[cfg(unix)]
    fn transcript_reads_accept_custom_redaction_policy() {
        let mut server = RpcServer::new();
        let create = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","printf 'internal-123 public'"]}}"#,
        );
        let session = create["result"]["session"].as_str().unwrap();
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"session.wait","params":{{"session":"{session}","matcher":{{"type":"contains_text","value":"internal-123"}},"timeout_ms":5000}}}}"#,
            ),
        );
        let redacted = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"session.transcript","params":{{"session":"{session}","redaction":{{"enabled":false,"replacement":"[X]","extra_regexes":["internal-[0-9]+"]}}}}}}"#,
            ),
        );

        assert_eq!(redacted["result"]["text"], "[X] public");
    }

    #[test]
    fn transcript_reads_reject_invalid_custom_redaction_regex() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"session.transcript","params":{"session":"missing","redaction":{"enabled":true,"replacement":"[REDACTED]","extra_regexes":["("]}}}"#,
        );

        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid redaction regex")
        );
    }

    #[test]
    fn session_create_rejects_append_without_raw_transcript_path() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"ptywright-test-missing","raw_transcript_append":true}}"#,
        );

        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("requires raw_transcript_path")
        );
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos()
    }

    #[test]
    fn serve_ndjson_writes_one_response_per_request_line() {
        let input = br#"{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}
{"jsonrpc":"2.0","id":2,"method":"session.list"}
"#;
        let mut output = Vec::new();

        serve_ndjson(&input[..], &mut output).expect("serve ndjson");

        let text = String::from_utf8(output).expect("utf8 output");
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["result"]["sessions"],
            json!([])
        );
    }

    #[test]
    fn serve_lsp_writes_content_length_frames() {
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"session.list"}"#;
        let input = format!("Content-Length: {}\r\n\r\n{}", request.len(), request);
        let mut output = Vec::new();

        serve_lsp(input.as_bytes(), &mut output).expect("serve lsp");

        let text = String::from_utf8(output).expect("utf8 output");
        let (headers, payload) = text.split_once("\r\n\r\n").expect("lsp separator");
        assert!(headers.contains("Content-Length:"));
        let response: Value = serde_json::from_str(payload).expect("json payload");
        assert_eq!(response["result"]["sessions"], json!([]));
    }

    #[test]
    fn serve_lsp_skips_notifications_without_response_frames() {
        let request = r#"{"jsonrpc":"2.0","method":"session.list"}"#;
        let input = format!("Content-Length: {}\r\n\r\n{}", request.len(), request);
        let mut output = Vec::new();

        serve_lsp(input.as_bytes(), &mut output).expect("serve lsp");

        assert!(output.is_empty());
    }
}
