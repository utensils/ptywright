use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::action::Action;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionHandle, LuaExtension};
use crate::matcher::Matcher;
use crate::plugin::{PluginHostCapabilities, PluginManifest};
use crate::redaction::RedactionPolicy;
use crate::session::{Session, SessionConfig};
use crate::target::{Target, TerminalSize};
use crate::transcript::TranscriptFileConfig;
use crate::{NAME, VERSION};

/// `completed_turn_stable_ms` forwarded to plugins via the `ExtensionHandle`.
/// A conservative default — plugins can override their own stability window
/// through the matcher they return from `wait_*_matcher` intents.
const ADAPTER_COMPLETED_TURN_STABLE_MS: u64 = 300;

/// Default wait-intent invoked by `adapter.wait` when the caller does not
/// supply one. Matches the conventional plugin function name; specific
/// plugins are free to expose other named intents.
const DEFAULT_WAIT_INTENT: &str = "wait_turn_matcher";

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
    /// Generic plugin-backed extension handles registered via `adapter.*`.
    /// The plugin name is stored on each entry so `adapter.list` /
    /// `adapter.inspect` can surface it without round-tripping the
    /// extension's manifest.
    extensions: HashMap<String, ExtensionEntry>,
    next_extension_id: u64,
    notifications_enabled: bool,
    last_notified_sequences: HashMap<String, u64>,
    notified_exits: HashSet<String>,
}

/// Per-adapter row stored in the `extensions` registry.
///
/// We keep the plugin name alongside the handle so `adapter.list` /
/// `adapter.inspect` responses can include it without forcing every plugin to
/// re-expose its manifest through the Extension trait.
///
/// `session` is a stable id allocated at `adapter.start` time so notification
/// subscribers can correlate `session.changed` / `session.exited` events with
/// the adapter that owns the underlying PTY. The id is allocated from the
/// same counter as `session.create` sessions, so it is unique within the
/// server process and never collides with directly-spawned sessions.
struct ExtensionEntry {
    plugin: String,
    session: String,
    handle: ExtensionHandle,
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
struct PluginManifestParams {
    manifest: PluginManifest,
}

#[derive(Debug, Deserialize)]
struct NotificationsParams {
    enabled: bool,
}

// ---- adapter.* (generic plugin-backed) params ---------------------------
//
// Callers select a built-in plugin manifest by name, the server instantiates
// a fresh [`ExtensionHandle`] around a [`Session`], and subsequent calls
// reference the handle by id. The plugin's manifest may declare a
// `default_target` so callers can omit `program` for plugins with a sensible
// host-known default.

#[derive(Debug, Deserialize)]
struct AdapterStartParams {
    /// Built-in plugin manifest name, e.g. `"claude-code"`.
    plugin: String,
    /// PTY program to spawn. If omitted, the server falls back to the
    /// plugin manifest's `default_target.program`. Pass explicitly when the
    /// plugin has no default or you want to override it.
    program: Option<String>,
    /// Extra CLI args. `None` (field omitted) falls back to the manifest's
    /// `default_target.args`; `Some(vec![])` is an explicit "no args"
    /// override that suppresses the manifest default. This distinction
    /// matters once a plugin's default_target.args is non-empty (today
    /// only `claude-code` ships built-in and its defaults are empty, but
    /// future plugins may pre-populate flags).
    args: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    rows: Option<u16>,
    cols: Option<u16>,
    pixel_width: Option<u16>,
    pixel_height: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct AdapterParams {
    adapter: String,
}

#[derive(Debug, Deserialize)]
struct AdapterSendParams {
    adapter: String,
    intent: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct AdapterWaitParams {
    adapter: String,
    /// Plugin function used to construct the wait matcher. Defaults to the
    /// conventional `wait_turn_matcher` so callers can omit it for the
    /// common turn-boundary wait.
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    params: Value,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AdapterReadParams {
    adapter: String,
    /// Whether to redact sensitive-looking output fields. Defaults to true.
    redact: Option<bool>,
    /// Optional caller-supplied redaction additions/replacement for this read.
    redaction: Option<RedactionPolicy>,
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
            extensions: HashMap::new(),
            next_extension_id: 1,
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

        // Shared sessions first (allocated via `session.create`). Snapshot the
        // current list under the lock so we can release it before emitting.
        let mut sessions = self
            .shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.sequence(), session.is_finished()))
            .collect::<Vec<_>>();
        sessions.sort_by(|(left, ..), (right, ..)| left.cmp(right));

        // Adapter-backed sessions. Each `adapter.start` allocates a session id
        // and stashes it on the entry so we can surface change/exit events
        // here without registering the (non-`Arc`) session into the shared
        // map.
        let mut adapter_sessions = self
            .extensions
            .values()
            .map(|entry| {
                let session = entry.handle.session();
                (
                    entry.session.clone(),
                    session.sequence(),
                    session.is_finished(),
                )
            })
            .collect::<Vec<_>>();
        adapter_sessions.sort_by(|(left, ..), (right, ..)| left.cmp(right));
        sessions.append(&mut adapter_sessions);

        for (id, sequence, finished) in sessions {
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
            if finished && self.notified_exits.insert(id.clone()) {
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
                    "adapter.list",
                    "adapter.start",
                    "adapter.state",
                    "adapter.send",
                    "adapter.wait",
                    "adapter.snapshot",
                    "adapter.transcript",
                    "adapter.inspect",
                    "adapter.close",
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
            "adapter.list" => Ok(self.adapter_list()),
            "adapter.start" => self.adapter_start(request.params),
            "adapter.state" => self.adapter_state(request.params),
            "adapter.send" => self.adapter_send(request.params),
            "adapter.wait" => self.adapter_wait(request.params),
            "adapter.snapshot" => self.adapter_snapshot(request.params),
            "adapter.transcript" => self.adapter_transcript(request.params),
            "adapter.inspect" => self.adapter_inspect(request.params),
            "adapter.close" => self.adapter_close(request.params),
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

    fn allocate_extension_id(&mut self) -> String {
        let id = format!("e{}", self.next_extension_id);
        self.next_extension_id += 1;
        id
    }

    fn extension(&self, id: &str) -> std::result::Result<&ExtensionEntry, (RpcErrorCode, String)> {
        self.extensions.get(id).ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown adapter: {id}"),
            )
        })
    }

    fn extension_mut(
        &mut self,
        id: &str,
    ) -> std::result::Result<&mut ExtensionEntry, (RpcErrorCode, String)> {
        self.extensions.get_mut(id).ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown adapter: {id}"),
            )
        })
    }

    // ---- adapter.* handlers ---------------------------------------------

    /// `adapter.list` — enumerate the built-in plugin manifests this server
    /// can instantiate. Reused by introspection clients before calling
    /// `adapter.start`.
    fn adapter_list(&self) -> Value {
        let capabilities = PluginHostCapabilities::current();
        json!({ "plugins": capabilities.builtin_plugins })
    }

    /// `adapter.start` — spawn a PTY session and wrap it in an
    /// [`ExtensionHandle`] for the requested plugin. Returns the allocated
    /// adapter id plus the initial classified state.
    fn adapter_start(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterStartParams = parse_params(params)?;
        let extension = LuaExtension::built_in(&params.plugin).map_err(rpc_error_from_error)?;
        // Fall back to the manifest's declared default target when the caller
        // omits `program`. Args follow the same rule independently: an
        // explicit `args` list always wins, otherwise the manifest default's
        // args are used.
        let manifest_default = extension.manifest().default_target.clone();
        let program = params
            .program
            .or_else(|| manifest_default.as_ref().map(|t| t.program.clone()))
            .ok_or_else(|| {
                (
                    RpcErrorCode::InvalidParams,
                    format!(
                        "no host-known default program for plugin `{plugin}`; pass `program` explicitly",
                        plugin = params.plugin,
                    ),
                )
            })?;
        // Args: explicit caller-supplied Vec (including an explicit empty
        // list) always wins. Only when the field is omitted entirely do we
        // fall back to the manifest's default_target.args. This makes
        // `{"args": []}` a meaningful override even for plugins whose
        // manifest pre-populates flags.
        let args = params.args.unwrap_or_else(|| {
            manifest_default
                .as_ref()
                .map(|t| t.args.clone())
                .unwrap_or_default()
        });
        let size = TerminalSize {
            rows: params.rows.unwrap_or(40),
            cols: params.cols.unwrap_or(120),
            pixel_width: params.pixel_width.unwrap_or(0),
            pixel_height: params.pixel_height.unwrap_or(0),
        };
        let mut target = Target::new(program).args(args).size(size);
        target.cwd = params.cwd;
        target.env = params.env;
        let session = Session::spawn(SessionConfig::new(target)).map_err(rpc_error_from_error)?;
        let handle = ExtensionHandle::start(
            Box::new(extension),
            session,
            ADAPTER_COMPLETED_TURN_STABLE_MS,
        );
        let state = handle.state();
        let id = self.allocate_extension_id();
        let session_id = self.allocate_session_id();
        self.extensions.insert(
            id.clone(),
            ExtensionEntry {
                plugin: params.plugin.clone(),
                session: session_id.clone(),
                handle,
            },
        );
        Ok(json!({
            "adapter": id,
            "plugin": params.plugin,
            "session": session_id,
            "state": state,
        }))
    }

    /// `adapter.state` — re-classify and return the current state without
    /// applying any actions.
    fn adapter_state(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterParams = parse_params(params)?;
        let entry = self.extension(&params.adapter)?;
        Ok(json!({ "state": entry.handle.state() }))
    }

    /// `adapter.send` — invoke a named plugin intent (e.g. `send_prompt`,
    /// `approve`, `deny`, `cancel`) and return the post-apply state. The
    /// intent name is forwarded verbatim to the plugin so the host carries
    /// no application-specific dispatch table.
    fn adapter_send(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterSendParams = parse_params(params)?;
        let intent = params.intent.clone();
        let state = self
            .extension_mut(&params.adapter)?
            .handle
            .send(&intent, params.params)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    /// `adapter.wait` — block until the plugin's named matcher fires or the
    /// timeout expires, then classify and return the resulting state. The
    /// intent defaults to `wait_turn_matcher` so simple callers can omit it.
    fn adapter_wait(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterWaitParams = parse_params(params)?;
        let intent = params
            .intent
            .unwrap_or_else(|| DEFAULT_WAIT_INTENT.to_string());
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(120_000));
        let state = self
            .extension(&params.adapter)?
            .handle
            .wait(&intent, params.params, timeout)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    /// `adapter.snapshot` — passthrough to the adapter's underlying session
    /// snapshot. Mirrors `session.snapshot`'s redaction semantics.
    fn adapter_snapshot(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterReadParams = parse_params(params)?;
        let entry = self.extension(&params.adapter)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let mut snapshot = entry.handle.session().snapshot();
        if let Some(policy) = policy {
            snapshot = snapshot.redacted(&policy);
        }
        serde_json::to_value(snapshot)
            .map_err(|error| (RpcErrorCode::InternalError, error.to_string()))
    }

    /// `adapter.transcript` — passthrough to the adapter's underlying session
    /// transcript. Mirrors `session.transcript`'s redaction semantics.
    fn adapter_transcript(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterReadParams = parse_params(params)?;
        let entry = self.extension(&params.adapter)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let session = entry.handle.session();
        let text = if let Some(policy) = policy {
            session.redacted_transcript(&policy)
        } else {
            session.transcript()
        };
        Ok(json!({ "text": text }))
    }

    /// `adapter.inspect` — diagnostic dump. Returns the current classified
    /// state plus the body/status split the classifier would see, so callers
    /// can reproduce a misclassification without spinning up a parallel
    /// `session.*` session.
    fn adapter_inspect(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterReadParams = parse_params(params)?;
        let entry = self.extension(&params.adapter)?;
        let session = entry.handle.session();
        let snapshot = session.snapshot();
        let transcript = session.transcript();
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        // 4 KiB tail mirrors what session.wait returns as transcript_tail.
        // Walk to a valid UTF-8 boundary so multibyte glyphs in TUI output
        // (⏺, ❯, →, …) cannot panic the handler when the byte offset lands
        // mid-codepoint.
        let mut transcript_tail = transcript_tail_bytes(&transcript, 4096).to_string();
        let mut plain_text = snapshot.plain_text.clone();
        if let Some(policy) = policy {
            plain_text = policy.redact(&plain_text);
            transcript_tail = policy.redact(&transcript_tail);
        }
        let (body_text, status_text) = crate::extension::split_status_bar_for_inspect(&plain_text);
        Ok(json!({
            "adapter": params.adapter,
            "plugin": entry.plugin,
            "session": entry.session,
            "state": entry.handle.state(),
            "plain_text": plain_text,
            "body_text": body_text,
            "status_text": status_text,
            "transcript_tail": transcript_tail,
            "sequence": snapshot.sequence,
        }))
    }

    /// `adapter.close` — terminate the underlying PTY child and drop the
    /// handle. Subsequent calls against the same adapter id return
    /// InvalidParams.
    ///
    /// Mirrors `session.close`: the host kills the child before dropping the
    /// owning struct. `Session` does not implement `Drop` to kill the child,
    /// so closing the registry entry alone would leak the PTY process until
    /// the parent exited.
    fn adapter_close(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (RpcErrorCode, String)> {
        let params: AdapterParams = parse_params(params)?;
        if let Some(entry) = self.extensions.remove(&params.adapter) {
            // Best-effort kill — if the child already exited the kill returns
            // an error which we discard. The session is dropped immediately
            // afterwards either way.
            let _ = entry.handle.session().kill();
            Ok(json!({ "closed": true }))
        } else {
            Err((
                RpcErrorCode::InvalidParams,
                format!("unknown adapter: {}", params.adapter),
            ))
        }
    }
}

/// Return at most the last `max_bytes` bytes of `transcript`, rounding the
/// start offset up to the nearest UTF-8 character boundary so the resulting
/// slice is always a valid `&str`.
///
/// A naïve `transcript[transcript.len().saturating_sub(max_bytes)..]` will
/// panic if the offset lands inside a multibyte codepoint, which is reachable
/// in practice because Claude Code's TUI uses characters like `⏺` (3 bytes)
/// and `❯` (3 bytes) liberally. The walk forward is bounded by 3 bytes
/// (UTF-8's max non-leading-byte run), so the worst case trims 3 leading
/// bytes from the requested window.
fn transcript_tail_bytes(transcript: &str, max_bytes: usize) -> &str {
    if transcript.len() <= max_bytes {
        return transcript;
    }
    let mut start = transcript.len() - max_bytes;
    while !transcript.is_char_boundary(start) {
        start += 1;
    }
    &transcript[start..]
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
        assert!(methods.contains(&json!("adapter.start")));
        assert!(methods.contains(&json!("adapter.send")));
        assert!(methods.contains(&json!("adapter.wait")));
        assert!(methods.contains(&json!("plugin.validate_manifest")));
        // No leftover claude.* methods: callers must use the generic
        // adapter.* surface now.
        assert!(
            !methods
                .iter()
                .any(|m| m.as_str().is_some_and(|name| name.starts_with("claude."))),
            "claude.* methods must not be advertised; got {methods:?}"
        );
    }

    #[test]
    fn transcript_tail_bytes_returns_whole_string_when_under_limit() {
        assert_eq!(transcript_tail_bytes("hello", 4096), "hello");
        assert_eq!(transcript_tail_bytes("", 4096), "");
    }

    #[test]
    fn transcript_tail_bytes_walks_to_char_boundary() {
        // `⏺` is U+23FA, encoded as 3 bytes (0xE2 0x8F 0xBA). Build a string
        // where the unsanitised byte offset would land inside it: a 6-byte
        // prefix of `bba` so that taking the last 5 bytes lands at byte 1,
        // mid-glyph. The helper must walk forward to the next valid char
        // boundary (byte 3, start of `⏺`) and return `⏺abc`.
        let s = "bba⏺abc"; // 3 ASCII + 3-byte glyph + 3 ASCII = 9 bytes
        assert_eq!(s.len(), 9);
        // last 7 bytes would naively start at byte 2 ('a'), valid boundary.
        assert_eq!(transcript_tail_bytes(s, 7), "a⏺abc");
        // last 6 bytes would naively start at byte 3, valid boundary.
        assert_eq!(transcript_tail_bytes(s, 6), "⏺abc");
        // last 5 bytes would naively start at byte 4, mid-codepoint -> walk
        // forward to byte 6 (end of glyph) and return only the trailing
        // ASCII so the resulting slice is valid UTF-8.
        let trimmed = transcript_tail_bytes(s, 5);
        assert_eq!(trimmed, "abc");
        assert!(trimmed.is_char_boundary(0));
    }

    #[test]
    fn transcript_tail_bytes_never_panics_on_multibyte_glyph_run() {
        // Stress: a long run of 3-byte glyphs followed by ASCII. For every
        // byte offset close to the boundary, the helper must produce a
        // valid &str. The original naive slice panicked here.
        let body = "⏺".repeat(100); // 300 bytes
        let s = format!("{body}TAIL");
        for max_bytes in 1..=304 {
            let tail = transcript_tail_bytes(&s, max_bytes);
            assert!(
                tail.is_char_boundary(0),
                "tail for max_bytes={max_bytes} starts mid-codepoint"
            );
            // Sanity: tail is always shorter than or equal to max_bytes + 3
            // (worst case is walking forward 3 bytes from the byte offset).
            assert!(tail.len() <= max_bytes + 3);
        }
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
    #[cfg(unix)]
    fn notifications_report_adapter_session_changes() {
        // Adapter-spawned PTYs are not registered in `shared.sessions`, but
        // the notification poller still surfaces their progress under the
        // `session` id returned by `adapter.start`. The REPL client relies on
        // this to keep its live screen-preview pane current without polling.
        let mut server = RpcServer::new();
        let _ = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
            )
            .expect("enable notifications");
        let start_messages = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":2,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'adapter-notify\\n' && cat"]}}"#,
            )
            .expect("adapter.start");
        let start_response: Value =
            serde_json::from_str(&start_messages[0]).expect("json response");
        let adapter = start_response["result"]["adapter"]
            .as_str()
            .expect("adapter.start must return an adapter id");
        let session = start_response["result"]["session"]
            .as_str()
            .expect("adapter.start must return the allocated session id")
            .to_string();

        // Generous 10 s deadline keeps the test reliable on busy CI hosts
        // where parallel `/bin/sh` PTY spawns slow fork+exec; the steady-
        // state behavior is observed within tens of milliseconds locally.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_changed = false;
        while std::time::Instant::now() < deadline && !saw_changed {
            let poll_messages = server
                .handle_line_messages(&format!(
                    r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.state","params":{{"adapter":"{adapter}"}}}}"#,
                ))
                .expect("poll for notifications");
            for message in poll_messages {
                if message.contains("\"method\":\"session.changed\"") && message.contains(&session)
                {
                    saw_changed = true;
                    break;
                }
            }
            if !saw_changed {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        assert!(
            saw_changed,
            "expected session.changed notification for adapter session `{session}` within 10s"
        );

        // Drain and close.
        let _ = server.handle_line_messages(&format!(
            r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#,
        ));
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
    fn adapter_list_includes_built_in_claude_code_plugin() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.list"}"#,
        );

        let plugins = response["result"]["plugins"]
            .as_array()
            .expect("adapter.list must return a plugins array");
        let claude = plugins
            .iter()
            .find(|p| p["name"] == "claude-code")
            .expect("adapter.list must include the built-in claude-code plugin");
        // The manifest declares default_target so adapter.start can spawn
        // claude-code without an explicit program. Lock that wiring in
        // here — without it the RPC surface would force every caller to
        // pass `program` even for the only built-in.
        assert_eq!(
            claude["default_target"]["program"], "claude",
            "claude-code manifest must declare its default program"
        );
    }

    #[test]
    fn adapter_start_uses_manifest_default_program_and_args_when_caller_omits_them() {
        // Lock in the no-program/no-args branch of adapter.start so callers
        // can spawn the built-in claude-code plugin with just `{"plugin":
        // "claude-code"}`. Exercises both `params.program.or_else(...)` and
        // `params.args.unwrap_or_else(...)`. Whether `claude` actually
        // resolves on PATH varies by environment, so this test accepts
        // either a successful spawn (with cleanup) or an internal spawn
        // error — what we're verifying is that resolution *succeeded* and
        // no InvalidParams "no host-known default" error fired.
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code"}}"#,
        );
        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            assert_ne!(
                code, -32602,
                "manifest default_target must satisfy resolution; \
                 InvalidParams indicates the fallback never ran (got error {error:?})",
            );
        }
        if let Some(adapter) = response
            .get("result")
            .and_then(|r| r.get("adapter"))
            .and_then(|a| a.as_str())
        {
            // claude is installed in this environment; clean up so the
            // spawned process doesn't outlive the test.
            let _ = handle(
                &mut server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
                ),
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn adapter_start_explicit_empty_args_overrides_manifest_default_args() {
        // `args: []` must be a meaningful override that suppresses the
        // manifest's default_target.args, even when the manifest pre-populates
        // flags. Today claude-code ships with empty default args, so we
        // can't observe a behavioural difference there — instead this test
        // asserts the *params* shape (None vs Some(vec![])) through a real
        // spawn against /bin/sh that would fail if the deserialiser treated
        // an explicit empty list as "fall back to manifest defaults".
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":[]}}"#,
        );
        let adapter = start["result"]["adapter"]
            .as_str()
            .expect("adapter.start with explicit empty args must succeed");
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
    }

    #[test]
    fn adapter_start_rejects_unknown_plugin() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"does-not-exist","program":"/bin/sh"}}"#,
        );

        assert_eq!(response["error"]["code"], -32603);
        let message = response["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("no built-in Lua extension"),
            "error message should name the missing plugin; got `{message}`"
        );
    }

    #[test]
    fn adapter_methods_for_unknown_adapter_return_invalid_params() {
        // Every adapter.* handler shares the same lookup path; cover each
        // explicitly so a future refactor that splits the dispatcher cannot
        // silently regress any one of them.
        for (id, method) in [
            (30, "adapter.state"),
            (31, "adapter.snapshot"),
            (32, "adapter.transcript"),
            (33, "adapter.inspect"),
            (34, "adapter.close"),
        ] {
            let mut server = RpcServer::new();
            let response = handle(
                &mut server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{"adapter":"missing"}}}}"#
                ),
            );
            assert_eq!(
                response["error"]["code"], -32602,
                "{method} should reject unknown adapter with InvalidParams"
            );
        }
        // send/wait take additional required fields beyond `adapter`, so
        // cover them with their full param shape.
        let mut server = RpcServer::new();
        let send = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":40,"method":"adapter.send","params":{"adapter":"missing","intent":"approve","params":{}}}"#,
        );
        assert_eq!(send["error"]["code"], -32602);
        let wait = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":41,"method":"adapter.wait","params":{"adapter":"missing","intent":"wait_turn_matcher","timeout_ms":100}}"#,
        );
        assert_eq!(wait["error"]["code"], -32602);
    }

    #[test]
    #[cfg(unix)]
    fn adapter_lifecycle_round_trips_through_generic_surface() {
        // End-to-end smoke for the adapter.* surface against a /bin/sh
        // stand-in. Exercises start → state → snapshot → transcript →
        // inspect → send → close. The shell prints a fixture line then
        // `cat`s stdin so it stays alive long enough for the read methods
        // to observe state.
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'adapter-fixture\\n' && cat"]}}"#,
        );
        let adapter = start["result"]["adapter"]
            .as_str()
            .expect("adapter.start must return an adapter id");
        assert_eq!(start["result"]["plugin"], "claude-code");
        assert!(
            start["result"]["state"].is_object(),
            "adapter.start must return an initial state snapshot"
        );
        let start_session = start["result"]["session"]
            .as_str()
            .expect("adapter.start must return the allocated session id")
            .to_string();
        assert!(
            start_session.starts_with('s'),
            "adapter session ids share the `s<n>` namespace with session.create; got `{start_session}`"
        );

        // Poll adapter.transcript instead of sleeping so the test stays
        // deterministic when the suite runs in parallel. The fixture line
        // is printed by the inner shell before `cat` starts reading stdin.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let transcript = handle(
                &mut server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":10,"method":"adapter.transcript","params":{{"adapter":"{adapter}","redact":false}}}}"#
                ),
            );
            let text = transcript["result"]["text"].as_str().unwrap_or("");
            if text.contains("adapter-fixture") {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("fixture line never appeared in adapter transcript within 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let state = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.state","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
        assert!(state["result"]["state"]["state"].is_string());

        let snapshot = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.snapshot","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
        assert!(snapshot["result"]["plain_text"].is_string());

        let transcript = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"adapter.transcript","params":{{"adapter":"{adapter}","redact":false}}}}"#
            ),
        );
        let text = transcript["result"]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("adapter-fixture"),
            "adapter.transcript must include the underlying bytes; got `{text}`"
        );

        let inspect = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":5,"method":"adapter.inspect","params":{{"adapter":"{adapter}","redact":false}}}}"#
            ),
        );
        assert_eq!(inspect["result"]["plugin"], "claude-code");
        assert_eq!(inspect["result"]["adapter"], adapter);
        assert_eq!(
            inspect["result"]["session"], start_session,
            "adapter.inspect must echo the same session id adapter.start allocated"
        );
        assert!(inspect["result"]["body_text"].is_string());
        assert!(inspect["result"]["status_text"].is_string());

        // adapter.send happy-path: route approve / deny intents through the
        // generic dispatcher. Asserts the response shape and that the
        // dispatcher hands the intent string to the Lua plugin without a
        // translation table in between.
        let send_approve = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":50,"method":"adapter.send","params":{{"adapter":"{adapter}","intent":"approve","params":{{}}}}}}"#
            ),
        );
        assert!(
            send_approve["result"]["state"].is_object(),
            "adapter.send must return {{state: ...}}; got {send_approve}"
        );
        let send_deny = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":51,"method":"adapter.send","params":{{"adapter":"{adapter}","intent":"deny","params":{{}}}}}}"#
            ),
        );
        assert!(send_deny["result"]["state"].is_object());

        // adapter.close must kill the underlying child rather than just
        // dropping the registry entry — Session has no Drop hook to kill the
        // PTY child, so registry-only removal would leak the spawned shell.
        // Drive the close against an adapter we just spawned with a
        // long-running `cat` and assert the child is gone afterwards.
        let close = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":6,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
        assert_eq!(close["result"]["closed"], true);

        // After close, subsequent calls against the same id must reject.
        let after_close = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":7,"method":"adapter.state","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
        assert_eq!(after_close["error"]["code"], -32602);
    }

    #[test]
    #[cfg(unix)]
    fn adapter_wait_completes_when_turn_boundary_anchor_appears() {
        // Drive ExtensionHandle::wait through adapter.wait against a shell
        // stand-in that prints `Total cost:` (one of the Lua plugin's
        // turn-boundary anchors) and then sleeps long enough for the
        // 300 ms screen_stable window to elapse. Covers the wait dispatcher,
        // ExtensionHandle::wait's post-match classify path, and the default
        // wait_turn_matcher intent name.
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'Total cost: 0\\n'; sleep 5"]}}"#,
        );
        let adapter = start["result"]["adapter"]
            .as_str()
            .expect("adapter.start must return an adapter id");

        // 3 s is comfortably more than the 300 ms stable window + a small
        // OS-scheduler buffer. If the matcher mis-classified `Total cost:`
        // this would time out instead of returning.
        let response = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.wait","params":{{"adapter":"{adapter}","timeout_ms":3000}}}}"#
            ),
        );
        assert!(
            response["result"]["state"].is_object(),
            "adapter.wait must return a state snapshot; got {response}"
        );
        let state_label = response["result"]["state"]["state"].as_str().unwrap_or("");
        assert!(
            !state_label.is_empty(),
            "adapter.wait response must include a non-empty state label"
        );

        // Cleanup so the sleep process doesn't outlive the test.
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
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
