use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::action::Action;
use crate::adapters::{ClaudeCodeAdapter, ClaudeCodeConfig};
use crate::error::{Error, Result};
use crate::matcher::Matcher;
use crate::plugin::{PluginHostCapabilities, PluginManifest};
use crate::session::{Session, SessionConfig};
use crate::target::{Target, TerminalSize};
use crate::{NAME, VERSION};

const JSONRPC_VERSION: &str = "2.0";

/// Stateful JSON-RPC handler for ptywright sessions.
pub struct RpcServer {
    sessions: HashMap<String, Session>,
    claude_adapters: HashMap<String, ClaudeCodeAdapter>,
    next_session: u64,
    next_claude_adapter: u64,
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
}

#[derive(Debug, Deserialize)]
struct SessionParams {
    session: String,
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

impl RpcServer {
    /// Create an empty JSON-RPC server state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            claude_adapters: HashMap::new(),
            next_session: 1,
            next_claude_adapter: 1,
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
                "framing": "ndjson",
                "methods": [
                    "server.capabilities",
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
                "notifications": []
            })),
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
        let session = Session::spawn(SessionConfig::new(target)).map_err(rpc_error_from_error)?;
        let id = self.allocate_session_id();
        self.sessions.insert(id.clone(), session);
        Ok(json!({ "session": id }))
    }

    fn session_list(&self) -> Value {
        let mut sessions = self.sessions.keys().cloned().collect::<Vec<_>>();
        sessions.sort();
        json!({ "sessions": sessions })
    }

    fn session_close(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionParams = parse_params(params)?;
        if let Some(session) = self.sessions.remove(&params.session) {
            let _ = session.kill();
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
        let params: SessionParams = parse_params(params)?;
        let snapshot = self.session(&params.session)?.snapshot();
        serde_json::to_value(snapshot)
            .map_err(|error| (RpcErrorCode::InternalError, error.to_string()))
    }

    fn session_transcript(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: SessionParams = parse_params(params)?;
        Ok(json!({ "text": self.session(&params.session)?.transcript() }))
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

    fn session(&self, id: &str) -> std::result::Result<&Session, (RpcErrorCode, String)> {
        self.sessions.get(id).ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown session: {id}"),
            )
        })
    }

    fn allocate_session_id(&mut self) -> String {
        let id = format!("s{}", self.next_session);
        self.next_session += 1;
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
pub fn serve_ndjson(input: impl Read, mut output: impl Write) -> Result<()> {
    let mut server = RpcServer::new();
    let reader = BufReader::new(input);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line)? {
            output.write_all(response.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn parse_params<T>(params: Option<Value>) -> std::result::Result<T, (RpcErrorCode, String)>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| (RpcErrorCode::InvalidParams, error.to_string()))
}

fn success_response(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn error_response(id: Option<Value>, code: RpcErrorCode, message: String) -> Value {
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
        Error::Pty(_) | Error::Io(_) | Error::Json(_) | Error::Rpc(_) => {
            RpcErrorCode::InternalError
        }
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
        assert_eq!(response["result"]["framing"], "ndjson");
        let methods = response["result"]["methods"].as_array().unwrap();
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
    fn plugin_validate_manifest_accepts_valid_manifest() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":10,"method":"plugin.validate_manifest","params":{"manifest":{"name":"demo","kind":"adapter","version":"0.1.0","permissions":["session.spawn","screen.read"]}}}"#,
        );

        assert_eq!(response["result"]["valid"], true);
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
}
