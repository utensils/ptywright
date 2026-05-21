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
use crate::lua_plugin::LuaPlugin;
use crate::matcher::Matcher;
use crate::plugin::{BUILTIN_PLUGINS, PluginHostCapabilities, PluginManifest, PluginPermission};
use crate::redaction::RedactionPolicy;
use crate::session::{Session, SessionConfig};
use crate::target::{Target, TerminalSize};
use crate::transcript::{TranscriptDelta, TranscriptFileConfig};
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
    /// Live `adapter.start`-spawned extensions, shared across connections
    /// so disconnecting one client does not kill PTYs another client
    /// (or the same client after re-connect) might still want to drive.
    /// Each entry is `Arc<Mutex<...>>` so handlers can clone the handle
    /// out of the shared registry, drop the outer lock, then serialize
    /// per-adapter access without blocking unrelated work.
    extensions: HashMap<String, Arc<Mutex<ExtensionEntry>>>,
    /// Read-only sibling map: adapter id → plugin manifest name. Updated
    /// in lock-step with `extensions` whenever an adapter starts or
    /// closes. Lets `plugin.unload` check live-adapter binding without
    /// per-adapter `Mutex<ExtensionEntry>` locks — a `try_lock` against a
    /// busy adapter (e.g. one in the middle of an `adapter.wait`) would
    /// silently report "not bound" and let the unload race past the
    /// documented guarantee. Reading the plugin name out of this map is
    /// O(1) and never contends with handler work.
    adapter_plugin: HashMap<String, String>,
    /// Read-only sibling map: adapter id → session id. Same lifecycle
    /// guarantees as `adapter_plugin` (updated in lock-step with
    /// `extensions`). Used by `resolve_notification_filter` to translate
    /// caller-supplied `adapters: ["e1"]` filters into the underlying
    /// session ids without taking the per-adapter mutex — that mutex is
    /// held by `adapter.wait` / `adapter.turn` for the entire wait, and
    /// silently dropping the binding here would cause "subscribed to
    /// adapter e1" to deliver zero events for the full duration of any
    /// in-flight wait on e1, the opposite of what callers asked for.
    adapter_session: HashMap<String, String>,
    /// Plugin manifests + Lua sources known to this server. Built-in
    /// plugins are seeded on construction; trusted-local third-party plugins
    /// arrive through CLI `--plugin <manifest.toml>` flags or the
    /// `plugin.load` JSON-RPC method.
    ///
    /// Keyed by manifest name. Adapter starts look this up to construct a
    /// fresh `LuaExtension` per `adapter.start` call without re-reading the
    /// source from disk.
    registered_plugins: HashMap<String, RegisteredPlugin>,
    /// Live plugin instances bound to every adapter session so
    /// `Matcher::Lua` predicates can resolve. Kept in lockstep with
    /// `registered_plugins` — entries land at registration time
    /// (`plugin.load` or built-in init) and disappear on `plugin.unload`.
    /// Shared via `Arc` so each adapter session can hold a stable
    /// handle without going through the shared-state mutex on every
    /// wait tick.
    plugin_registry: Arc<crate::lua_plugin::LuaPluginRegistry>,
    /// Client-minted wait_id → CancellationToken for in-flight
    /// `adapter.wait` / `adapter.turn` calls. `adapter.cancel_wait`
    /// looks up the entry, flips the token, and lets the originating
    /// wait return `Error::Cancelled`. Entries are inserted before
    /// the wait blocks and removed when it returns (cancelled,
    /// matched, or timed out — all three paths clean up).
    pending_waits: HashMap<String, PendingWait>,
    /// Whether the `plugin.load` / `plugin.unload` RPC methods are enabled.
    /// `false` by default — operators opt in with `ptywright serve
    /// --allow-plugin-load`. CLI `--plugin` flags work regardless because
    /// the operator is loading plugins out-of-band at server startup.
    allow_plugin_load: bool,
    next_session: u64,
    next_extension: u64,
}

/// RAII guard that removes a `pending_waits` entry on drop.
///
/// Both successful matches and cancellation paths need to clean up;
/// a guard removes the boilerplate from every `Result` exit in
/// `adapter_wait` / `adapter_turn`. Holding a clone of
/// `RpcServerState.shared` keeps the inner mutex reachable from
/// Drop. The `wait_id` is `Option` so the same guard type covers the
/// "no cancellation requested" branch with a no-op drop.
struct PendingWaitCleanup {
    shared: RpcServerState,
    wait_id: Option<String>,
}

impl Drop for PendingWaitCleanup {
    fn drop(&mut self) {
        let Some(id) = self.wait_id.as_ref() else {
            return;
        };
        if let Ok(mut shared) = self.shared.inner.lock() {
            shared.pending_waits.remove(id);
        }
    }
}

/// One entry in [`RpcSharedState::pending_waits`].
///
/// `adapter` is recorded alongside the cancellation token so
/// notifications and diagnostics can attribute a cancelled wait back
/// to the adapter it was running against; v1 of `adapter.cancel_wait`
/// looks up by `wait_id` alone, since the id is globally unique.
struct PendingWait {
    adapter: String,
    token: crate::session::CancellationToken,
}

/// One entry in [`RpcSharedState::registered_plugins`].
///
/// `builtin: true` plugins are bundled into the binary and refused by
/// `plugin.unload` — operators cannot unload claude-code through the wire.
#[derive(Clone)]
struct RegisteredPlugin {
    manifest: PluginManifest,
    source: String,
    /// Auxiliary module sources to pre-load as Lua globals before the
    /// main `source` chunk runs. Empty for trusted-local third-party
    /// plugins loaded via `plugin.load` (they have to inline everything
    /// into their `main.lua` until the manifest format grows a
    /// `[[modules]]` array). Built-in plugins forward
    /// `BuiltinPlugin::modules` here so the same `LuaPlugin::trusted_with_modules`
    /// loader is used uniformly.
    modules: Vec<(String, String)>,
    builtin: bool,
}

impl Default for RpcSharedState {
    fn default() -> Self {
        let mut registered_plugins = HashMap::new();
        let plugin_registry = Arc::new(crate::lua_plugin::LuaPluginRegistry::new());
        for entry in BUILTIN_PLUGINS {
            let manifest = (entry.manifest)();
            let registered = RegisteredPlugin {
                manifest: manifest.clone(),
                source: entry.source.to_string(),
                modules: entry
                    .modules
                    .iter()
                    .map(|(name, source)| ((*name).to_string(), (*source).to_string()))
                    .collect(),
                builtin: true,
            };
            // Seed the live registry with a freshly-loaded LuaPlugin
            // for predicate evaluation. A registry load failure here
            // would indicate a malformed embedded plugin, which is a
            // build-time bug — log + skip so the server still boots
            // (legacy waits don't depend on Lua predicates).
            match build_registry_plugin(&registered) {
                Ok(plugin) => {
                    plugin_registry.insert(manifest.name.clone(), plugin);
                }
                Err(error) => {
                    tracing::error!(
                        target: "ptywright::rpc",
                        plugin = %manifest.name,
                        ?error,
                        "failed to seed built-in plugin into LuaPluginRegistry; \
                         Matcher::Lua predicates against this plugin will not fire"
                    );
                }
            }
            registered_plugins.insert(manifest.name.clone(), registered);
        }
        Self {
            sessions: HashMap::new(),
            extensions: HashMap::new(),
            adapter_plugin: HashMap::new(),
            adapter_session: HashMap::new(),
            registered_plugins,
            plugin_registry,
            pending_waits: HashMap::new(),
            allow_plugin_load: false,
            next_session: 1,
            next_extension: 1,
        }
    }
}

/// Load a [`RegisteredPlugin`]'s manifest + source into a fresh
/// [`LuaPlugin`] instance suitable for insertion into a
/// [`crate::lua_plugin::LuaPluginRegistry`]. Same code path as
/// `adapter.start` uses for the adapter-side instance — keeping
/// them in lockstep avoids a class of "predicate sees a different
/// plugin than the adapter" bugs at the source.
fn build_registry_plugin(entry: &RegisteredPlugin) -> Result<crate::lua_plugin::LuaPlugin> {
    let module_refs: Vec<(&str, &str)> = entry
        .modules
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect();
    crate::lua_plugin::LuaPlugin::trusted_with_modules(&entry.manifest, &entry.source, &module_refs)
}

/// Stateful JSON-RPC handler for one client connection.
pub struct RpcServer {
    shared: RpcServerState,
    notifications_enabled: bool,
    /// Per-connection notification scope. `None` (or both fields empty)
    /// means "fire for every session." Set by `server.set_notifications`
    /// when callers supply `adapters` and/or `sessions` filters.
    notification_filter: NotificationFilter,
    last_notified_sequences: HashMap<String, u64>,
    /// Per-session cursor into the transcript's monotonic `chars_written`
    /// counter. Used to drive `session.output` notifications: each tick we
    /// read the delta since the cursor and advance it, regardless of any
    /// ring-buffer evictions on the producer side (those surface as
    /// `dropped: true` in the notification payload). One cursor per
    /// connection — each `RpcServer` instance is its own subscriber and
    /// independently catches up after `server.set_notifications`.
    last_notified_outputs: HashMap<String, u64>,
    notified_exits: HashSet<String>,
}

/// Per-connection notification scope. Empty filters mean "everything"
/// — that's the back-compat path for callers that just toggle the
/// boolean. Non-empty filters fire for sessions matching `sessions`
/// OR whose owning adapter id is in `adapters` (UNION semantics).
#[derive(Debug, Default, Clone)]
struct NotificationFilter {
    adapters: Vec<String>,
    sessions: Vec<String>,
}

impl NotificationFilter {
    fn is_empty(&self) -> bool {
        self.adapters.is_empty() && self.sessions.is_empty()
    }
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
struct PluginLoadParams {
    /// Absolute or relative path to a TOML plugin manifest. The entrypoint
    /// declared in the manifest is read relative to the manifest's parent
    /// directory.
    manifest_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PluginUnloadParams {
    /// Manifest name to remove from the registry. Built-in plugins cannot
    /// be unloaded.
    plugin: String,
}

#[derive(Debug, Deserialize)]
struct PluginDescribeParams {
    /// Manifest name to introspect. Built-in or registered third-party
    /// plugins are both fair game.
    plugin: String,
}

#[derive(Debug, Deserialize)]
struct NotificationsParams {
    enabled: bool,
    /// Optional list of adapter ids to scope notifications to. Empty
    /// or omitted means "all adapters" (back-compat with the original
    /// boolean-only param). Adapter ids that don't match any live
    /// adapter at filter time are silently ignored — late binding lets
    /// callers pre-subscribe before `adapter.start` returns.
    #[serde(default)]
    adapters: Option<Vec<String>>,
    /// Optional list of session ids to scope notifications to. Combined
    /// with `adapters` as a UNION: events fire for sessions whose id
    /// matches `sessions` OR whose owning adapter id matches `adapters`.
    /// Empty or omitted means "all sessions".
    #[serde(default)]
    sessions: Option<Vec<String>>,
}

/// One session's notification-relevant snapshot for a single `poll_notifications`
/// tick. Collected in [`RpcServer::collect_notification_entries`] so the dispatch
/// loop emits at most one `session.changed`, one `session.output`, and one
/// `session.exited` per session per tick — and never re-reads session state
/// while emitting.
struct SessionNotificationEntry {
    id: String,
    sequence: u64,
    finished: bool,
    /// Transcript delta since the per-connection cursor, or `None` when the
    /// cursor is already at `chars_written`. Carries its own next-cursor in
    /// `TranscriptDelta::cursor` so the emitter advances atomically with the
    /// payload it sent.
    output: Option<TranscriptDelta>,
}

/// Pair of cloned-out registry views taken under one short lock.
/// See [`RpcServer::snapshot_session_registry`].
struct SessionRegistrySnapshot {
    direct: Vec<(String, Arc<Session>)>,
    adapter_arcs: Vec<Arc<Mutex<ExtensionEntry>>>,
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

/// Parameters for `adapter.resume`. Mirrors `AdapterStartParams` plus an
/// optional `prior_adapter` so the host can close the old PTY before
/// spawning the replacement. Keeping the field-by-field shape (rather
/// than embedding `AdapterStartParams`) keeps the wire schema stable
/// regardless of which struct the JSON-RPC dispatcher routes to.
#[derive(Debug, Deserialize)]
struct AdapterResumeParams {
    /// Built-in or trusted-local plugin manifest name.
    plugin: String,
    /// PTY program. Falls back to the plugin manifest's `default_target.program`.
    program: Option<String>,
    /// CLI args, e.g. `["--resume", "<uuid>"]`. Same `None` vs `Some(vec![])`
    /// distinction as `adapter.start`.
    args: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    rows: Option<u16>,
    cols: Option<u16>,
    pixel_width: Option<u16>,
    pixel_height: Option<u16>,
    /// Adapter id from a previous session, if any. When provided and still
    /// in the registry, the host closes it before spawning the
    /// replacement so callers don't have to make a separate
    /// `adapter.close` call. Missing ids are silently ignored (the caller
    /// may have already closed, or the prior adapter may never have
    /// started) — that keeps the call idempotent.
    prior_adapter: Option<String>,
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
    /// Optional client-minted identifier for the wait. When supplied,
    /// the server registers a `CancellationToken` keyed by this id so
    /// `adapter.cancel_wait { wait_id }` from any connection can
    /// abort the wait early. Must be unique across all currently
    /// in-flight waits on the server — duplicates are rejected with
    /// `-32602 InvalidParams`.
    #[serde(default)]
    wait_id: Option<String>,
}

/// Atomic send-then-wait used by [`RpcServer::adapter_turn`].
///
/// Modelled as nested `send` / `wait` sub-objects (rather than flat
/// duplicated fields) so the wire shape stays unambiguous when the same
/// adapter has overlapping intent / wait param vocabularies.
#[derive(Debug, Deserialize)]
struct AdapterTurnParams {
    adapter: String,
    send: TurnSendParams,
    #[serde(default)]
    wait: TurnWaitParams,
}

#[derive(Debug, Default, Deserialize)]
struct TurnSendParams {
    intent: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Default, Deserialize)]
struct TurnWaitParams {
    /// Optional plugin matcher function. Defaults to `wait_turn_matcher`
    /// so simple turns can omit the `wait` block entirely.
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    params: Value,
    timeout_ms: Option<u64>,
    /// Optional client-minted wait identifier. Same contract as
    /// [`AdapterWaitParams::wait_id`] — registers a cancellable token
    /// so `adapter.cancel_wait { wait_id }` from any connection can
    /// interrupt the wait leg of the turn.
    #[serde(default)]
    wait_id: Option<String>,
}

/// Params for `adapter.cancel_wait`. Identifies an in-flight wait by
/// its client-minted `wait_id` and flips the bound
/// [`crate::CancellationToken`] so the originating
/// `adapter.wait` / `adapter.turn` call returns
/// [`crate::Error::Cancelled`].
#[derive(Debug, Deserialize)]
struct AdapterCancelWaitParams {
    /// Client-minted identifier passed to the originating
    /// `adapter.wait` or `adapter.turn` via its `wait_id` field.
    wait_id: String,
}

#[derive(Debug, Deserialize)]
struct AdapterReadParams {
    adapter: String,
    /// Whether to redact sensitive-looking output fields. Defaults to true.
    redact: Option<bool>,
    /// Optional caller-supplied redaction additions/replacement for this read.
    redaction: Option<RedactionPolicy>,
}

/// Dispatcher-internal error payload.
///
/// Carries the JSON-RPC error code, the human-readable message, and an
/// optional structured `data` value that surfaces on the wire. Callers that
/// don't need `data` (the common case) construct via the `From<(RpcErrorCode,
/// String)>` impl so existing 2-tuple call sites coerce automatically through
/// `?` and `.into()`.
#[derive(Debug)]
struct RpcErrorPayload {
    code: RpcErrorCode,
    message: String,
    data: Option<Value>,
}

impl From<(RpcErrorCode, String)> for RpcErrorPayload {
    fn from((code, message): (RpcErrorCode, String)) -> Self {
        Self {
            code,
            message,
            data: None,
        }
    }
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
    PermissionDenied,
    /// Wait was aborted by a [`CancellationToken`] from another thread or
    /// connection. Distinct from `Timeout` so clients can tell "we ran
    /// out of time" from "another thread aborted us" — same distinction
    /// the underlying [`crate::Error::Cancelled`] makes inside the
    /// library.
    Cancelled,
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
            Self::PermissionDenied => -32004,
            Self::Cancelled => -32005,
        }
    }
}

/// Per-method permission requirements for the `adapter.*` JSON-RPC surface.
///
/// Lookup is `O(n)` but `n` is tiny and the table is the single source of
/// truth: every entry here corresponds to a method handler that calls
/// [`RpcServer::check_adapter_permission`] (or, for `adapter.start`, a direct
/// permission check on the manifest before the handle exists).
///
/// Methods that do not appear in this table are read-only registry queries
/// (e.g. `adapter.list`, `adapter.live`) and are allow-by-default. `session.*`
/// methods operate on directly-created sessions that have no associated plugin
/// manifest, so they are not gated here — third-party plugin permissioning of
/// `session.*` is tracked in the ongoing hardening backlog.
const ADAPTER_METHOD_PERMISSIONS: &[(&str, PluginPermission)] = &[
    ("adapter.start", PluginPermission::SessionSpawn),
    ("adapter.resume", PluginPermission::SessionSpawn),
    ("adapter.send", PluginPermission::InputWrite),
    ("adapter.wait", PluginPermission::MatcherWait),
    ("adapter.snapshot", PluginPermission::ScreenRead),
    ("adapter.transcript", PluginPermission::TranscriptRead),
    ("adapter.inspect", PluginPermission::ScreenRead),
    ("adapter.state", PluginPermission::ScreenRead),
    ("adapter.close", PluginPermission::SessionKill),
];

/// Look up the required permission for `method`, if any.
fn required_permission_for(method: &str) -> Option<PluginPermission> {
    ADAPTER_METHOD_PERMISSIONS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, perm)| perm.clone())
}

impl RpcServerState {
    /// Create an empty shared JSON-RPC server state seeded with every
    /// built-in plugin from [`BUILTIN_PLUGINS`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a trusted-local third-party plugin so callers can spawn it
    /// through `adapter.start` (or list it via `adapter.list`). Validates the
    /// manifest before insertion and rejects name collisions with already
    /// registered plugins.
    ///
    /// Used by the CLI when callers pass `--plugin <path/to/manifest.toml>`
    /// to `ptywright serve`, and by the `plugin.load` JSON-RPC handler when
    /// `--allow-plugin-load` is set.
    pub fn register_plugin(&self, manifest: PluginManifest, source: String) -> Result<()> {
        manifest
            .validate()
            .map_err(|error| Error::Config(error.to_string()))?;
        let name = manifest.name.clone();
        let mut shared = self.inner.lock().expect("rpc shared state poisoned");
        if shared.registered_plugins.contains_key(&name) {
            return Err(Error::Config(format!(
                "plugin `{name}` is already registered"
            )));
        }
        let registered = RegisteredPlugin {
            manifest,
            source,
            modules: Vec::new(),
            builtin: false,
        };
        // Mirror the registration into the live plugin registry so
        // subsequent `Matcher::Lua` predicate evaluations resolve.
        // Failure to build the registry instance is a hard error —
        // unlike the seed path (which only logs because the server
        // must boot), `register_plugin` is called interactively and
        // the caller can surface the failure to the operator.
        let registry_plugin = build_registry_plugin(&registered)?;
        shared.plugin_registry.insert(name.clone(), registry_plugin);
        shared.registered_plugins.insert(name, registered);
        Ok(())
    }

    /// Enable or disable the `plugin.load` / `plugin.unload` JSON-RPC
    /// methods. Off by default. Operators opt in with the
    /// `--allow-plugin-load` CLI flag on `ptywright serve`.
    pub fn set_allow_plugin_load(&self, allow: bool) {
        let mut shared = self.inner.lock().expect("rpc shared state poisoned");
        shared.allow_plugin_load = allow;
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
            notifications_enabled: false,
            notification_filter: NotificationFilter::default(),
            last_notified_sequences: HashMap::new(),
            last_notified_outputs: HashMap::new(),
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
                    (RpcErrorCode::ParseError, format!("parse error: {error}")).into(),
                ))
                .map(Some);
            }
        };

        let id = request.id.clone();
        let response_required = id.is_some();
        let response = match self.handle_request(request) {
            Ok(result) => success_response(id, result),
            Err(payload) => error_response(id, payload),
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
        let allowed_ids = self.resolve_notification_filter();
        let mut entries = self.collect_notification_entries();
        if let Some(allowed) = allowed_ids.as_ref() {
            entries.retain(|entry| allowed.contains(&entry.id));
        }
        entries.sort_by(|left, right| left.id.cmp(&right.id));

        for entry in entries {
            let SessionNotificationEntry {
                id,
                sequence,
                finished,
                output,
            } = entry;

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

            if let Some(delta) = output {
                self.last_notified_outputs.insert(id.clone(), delta.cursor);
                // `sequence` here is the session's *current* sequence at poll
                // time. If multiple PTY writes landed between polls, the
                // delivered `output` may span several sequence increments
                // but only the latest counter is reported — the notification
                // batches "everything since the last poll," not "everything
                // produced at this exact sequence." Subscribers correlating
                // byte ranges to specific sequence values should treat the
                // sequence here as an upper bound, not an exact alignment.
                let mut params = json!({
                    "session": id,
                    "sequence": sequence,
                    "output": delta.text,
                });
                if delta.dropped {
                    params
                        .as_object_mut()
                        .expect("params object")
                        .insert("dropped".to_string(), json!(true));
                }
                messages.push(serialize_response(json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "method": "session.output",
                    "params": params,
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

    /// Snapshot every session (directly-spawned and adapter-backed) into the
    /// data we need to emit a single notification batch: id, current
    /// sequence, finished flag, and the transcript delta since this
    /// connection's cursor.
    ///
    /// **Locking note.** Adapter entries are read with `try_lock` because a
    /// long-running `adapter.wait` holds the same per-entry mutex for the
    /// entire wait (up to `timeout_ms`, default 120 s). A blocking `lock()`
    /// here would stall every other connection's `session.*` flow behind a
    /// single contended adapter. Skipping a busy entry means we miss one
    /// tick — the next inbound request re-polls and catches up.
    fn collect_notification_entries(&self) -> Vec<SessionNotificationEntry> {
        let mut entries = Vec::new();
        let snapshot = self.snapshot_session_registry();

        for (id, session) in snapshot.direct {
            entries.push(self.build_notification_entry(id, session.as_ref()));
        }

        for arc in snapshot.adapter_arcs {
            let Ok(entry) = arc.try_lock() else { continue };
            let id = entry.session.clone();
            let session = entry.handle.session();
            entries.push(self.build_notification_entry(id, session));
        }

        entries
    }

    /// One-lock snapshot of the shared session registry. Cloning the Arcs out
    /// under a brief lock lets the caller iterate without holding the registry
    /// lock across per-session reads (which take their own internal locks and
    /// would otherwise serialise behind it).
    fn snapshot_session_registry(&self) -> SessionRegistrySnapshot {
        let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        SessionRegistrySnapshot {
            direct: shared
                .sessions
                .iter()
                .map(|(id, session)| (id.clone(), Arc::clone(session)))
                .collect(),
            adapter_arcs: shared.extensions.values().cloned().collect(),
        }
    }

    fn build_notification_entry(&self, id: String, session: &Session) -> SessionNotificationEntry {
        let sequence = session.sequence();
        let finished = session.is_finished();
        let cursor = self.last_notified_outputs.get(&id).copied().unwrap_or(0);
        // Notifications fire without a caller-supplied redaction policy, so
        // we apply the host default — same policy `adapter.transcript`
        // applies when `redact` is omitted. Subscribers that want raw bytes
        // can still call `adapter.transcript { redact: false }`.
        let delta = session.redacted_transcript_delta_since(cursor, &RedactionPolicy::default());
        let output = (delta.cursor > cursor).then_some(delta);
        SessionNotificationEntry {
            id,
            sequence,
            finished,
            output,
        }
    }

    fn handle_request(&mut self, request: Request) -> std::result::Result<Value, RpcErrorPayload> {
        if request.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
            return Err((
                RpcErrorCode::InvalidRequest,
                "jsonrpc must be \"2.0\"".to_string(),
            )
                .into());
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
                    "adapter.live",
                    "adapter.start",
                    "adapter.resume",
                    "adapter.state",
                    "adapter.send",
                    "adapter.wait",
                    "adapter.cancel_wait",
                    "adapter.turn",
                    "adapter.snapshot",
                    "adapter.transcript",
                    "adapter.inspect",
                    "adapter.close",
                    "plugin.capabilities",
                    "plugin.validate_manifest",
                    "plugin.describe",
                    "plugin.load",
                    "plugin.unload"
                ],
                "notifications": ["session.changed", "session.output", "session.exited"]
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
            "adapter.live" => Ok(self.adapter_live()),
            "adapter.start" => self.adapter_start(request.params),
            "adapter.resume" => self.adapter_resume(request.params),
            "adapter.state" => self.adapter_state(request.params),
            "adapter.send" => self.adapter_send(request.params),
            "adapter.wait" => self.adapter_wait(request.params),
            "adapter.cancel_wait" => self.adapter_cancel_wait(request.params),
            "adapter.turn" => self.adapter_turn(request.params),
            "adapter.snapshot" => self.adapter_snapshot(request.params),
            "adapter.transcript" => self.adapter_transcript(request.params),
            "adapter.inspect" => self.adapter_inspect(request.params),
            "adapter.close" => self.adapter_close(request.params),
            "plugin.capabilities" => Ok(json!(PluginHostCapabilities::current())),
            "plugin.validate_manifest" => self.plugin_validate_manifest(request.params),
            "plugin.describe" => self.plugin_describe(request.params),
            "plugin.load" => self.plugin_load(request.params),
            "plugin.unload" => self.plugin_unload(request.params),
            _ => Err((
                RpcErrorCode::MethodNotFound,
                format!("unknown method: {method}"),
            )
                .into()),
        }
    }

    fn server_set_notifications(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: NotificationsParams = parse_params(params)?;
        let was_enabled = self.notifications_enabled;
        self.notifications_enabled = params.enabled;
        self.notification_filter = NotificationFilter {
            adapters: params.adapters.unwrap_or_default(),
            sessions: params.sessions.unwrap_or_default(),
        };
        // When notifications transition from off → on, seed every existing
        // session's output cursor at the current `chars_written` so the next
        // `session.output` notification only carries output produced *after*
        // subscription. Without this, a long-lived REPL that just enabled
        // notifications would receive the entire retained transcript (up to
        // `max_chars` ≈ 128 KiB by default) crammed into a single NDJSON
        // frame — heavy on slow consumers and on any framing layer with a
        // line-length limit. Subscribers that *want* the historical buffer
        // can still call `adapter.transcript` explicitly.
        if !was_enabled && params.enabled {
            self.seed_output_cursors_at_current_position();
        }
        // Echo the resolved filter back so callers can confirm what the
        // server saw. Empty arrays are omitted to keep the response tidy
        // when no filter is in force.
        let mut resp = json!({ "enabled": self.notifications_enabled });
        if !self.notification_filter.adapters.is_empty() {
            resp.as_object_mut().unwrap().insert(
                "adapters".to_string(),
                json!(self.notification_filter.adapters),
            );
        }
        if !self.notification_filter.sessions.is_empty() {
            resp.as_object_mut().unwrap().insert(
                "sessions".to_string(),
                json!(self.notification_filter.sessions),
            );
        }
        Ok(resp)
    }

    /// Resolve the current notification filter into a concrete set of
    /// session ids. Returns `None` when no filter is in force (= deliver
    /// every session). Adapter ids in the filter are resolved against
    /// the live extension registry at call time; ids that don't match
    /// anything are silently ignored, so a caller can pre-subscribe to
    /// `adapters: ["e1"]` before the corresponding `adapter.start` call
    /// completes and the session-id mapping arrives later.
    ///
    /// Reads through the `adapter_session` sibling map rather than
    /// locking each `ExtensionEntry`. The per-adapter mutex is held
    /// for the entire duration of `adapter.wait` / `adapter.turn`
    /// (up to two minutes by default); a `try_lock` against a busy
    /// adapter would silently drop its session id from the filter and
    /// the caller would receive zero events for that adapter for the
    /// whole wait — exactly the opposite of what
    /// `set_notifications { adapters: [...] }` asked for. The mirror
    /// is maintained in lock-step with `extensions` so this lookup is
    /// O(1) and free of per-adapter contention.
    fn resolve_notification_filter(&self) -> Option<HashSet<String>> {
        if self.notification_filter.is_empty() {
            return None;
        }
        let mut session_ids: HashSet<String> =
            self.notification_filter.sessions.iter().cloned().collect();
        if !self.notification_filter.adapters.is_empty() {
            let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            for adapter_id in &self.notification_filter.adapters {
                if let Some(session_id) = shared.adapter_session.get(adapter_id) {
                    session_ids.insert(session_id.clone());
                }
            }
        }
        Some(session_ids)
    }

    fn seed_output_cursors_at_current_position(&mut self) {
        let snapshot = self.snapshot_session_registry();
        for (id, session) in snapshot.direct {
            let cursor = session.transcript_chars_written();
            self.last_notified_outputs.insert(id, cursor);
        }
        for arc in snapshot.adapter_arcs {
            let Ok(entry) = arc.try_lock() else { continue };
            let cursor = entry.handle.session().transcript_chars_written();
            self.last_notified_outputs
                .insert(entry.session.clone(), cursor);
        }
    }

    fn session_create(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
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
            )
                .into());
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
    ) -> std::result::Result<Value, RpcErrorPayload> {
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
            self.forget_session_notification_state(&params.session);
            Ok(json!({ "closed": true }))
        } else {
            Err((
                RpcErrorCode::InvalidParams,
                format!("unknown session: {}", params.session),
            )
                .into())
        }
    }

    fn session_kill(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: SessionParams = parse_params(params)?;
        let session = self.session(&params.session)?;
        session.kill().map_err(rpc_error_from_error)?;
        Ok(json!({ "killed": true }))
    }

    fn session_resize(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
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
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: InputParams = parse_params(params)?;
        self.session(&params.session)?
            .send(params.action)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "sent": true }))
    }

    fn session_snapshot(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
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
            .map_err(|error| (RpcErrorCode::InternalError, error.to_string()).into())
    }

    fn session_transcript(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
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

    fn session_wait(&self, params: Option<Value>) -> std::result::Result<Value, RpcErrorPayload> {
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
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: PluginManifestParams = parse_params(params)?;
        params
            .manifest
            .validate()
            .map_err(|error| -> RpcErrorPayload {
                (RpcErrorCode::InvalidParams, error.to_string()).into()
            })?;
        Ok(json!({ "valid": true }))
    }

    /// `plugin.describe` — return the catalog of a plugin's intents, wait
    /// matchers, classifier states, and manifest. Used by consumers
    /// (claudette, the REPL completer) to discover what a plugin supports
    /// without hard-coding intent names.
    ///
    /// Source of truth, in order:
    ///   1. If the plugin's Lua source exports a `describe()` function,
    ///      its return value is used verbatim. Plugins owning a richer
    ///      catalog (per-intent param schemas, per-state descriptions)
    ///      should provide it.
    ///   2. Otherwise the host falls back to introspecting the exports
    ///      table: function names ending with `_matcher` are listed under
    ///      `wait_matchers`; `classify` and `describe` are filtered out;
    ///      every remaining function is listed under `intents`. `states`
    ///      is `[]` in the fallback path because the classifier vocabulary
    ///      lives inside the function body.
    fn plugin_describe(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: PluginDescribeParams = parse_params(params)?;
        let entry = {
            let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            shared.registered_plugins.get(&params.plugin).cloned()
        };
        let Some(entry) = entry else {
            return Err((
                RpcErrorCode::InvalidParams,
                format!("unknown plugin: {}", params.plugin),
            )
                .into());
        };
        // Build a one-off LuaPlugin to introspect. Cheap — the source
        // is already in memory and Lua 5.4 + mlua loads a small chunk
        // in milliseconds; the alternative (caching a long-lived Lua
        // state per plugin) trades memory for cold-call latency in a
        // method that's only ever called interactively.
        let modules: Vec<(&str, &str)> = entry
            .modules
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_str()))
            .collect();
        let plugin = crate::lua_plugin::LuaPlugin::trusted_with_modules(
            &entry.manifest,
            &entry.source,
            &modules,
        )
        .map_err(rpc_error_from_error)?;
        let catalog = if plugin
            .exports_function("describe")
            .map_err(rpc_error_from_error)?
        {
            // Plugin provided its own catalog. Trust it verbatim — the
            // shape is `{ intents, wait_matchers, states }` (each an
            // array of objects with at minimum a `name` field). Plugins
            // may include richer fields (params_schema, description)
            // that downstream consumers can opt into.
            let value: Value = plugin
                .call_value("describe", &Value::Null)
                .map_err(rpc_error_from_error)?;
            value
        } else {
            // Introspection fallback. Anchor naming convention:
            //   * `_matcher` suffix → wait matcher
            //   * `classify` / `describe` → reserved, filtered out
            //   * everything else → intent
            let names = plugin
                .exported_function_names()
                .map_err(rpc_error_from_error)?;
            let mut intents: Vec<Value> = Vec::new();
            let mut wait_matchers: Vec<Value> = Vec::new();
            for name in names {
                if name == "classify" || name == "describe" {
                    continue;
                }
                if name.ends_with("_matcher") {
                    wait_matchers.push(json!({ "name": name }));
                } else {
                    intents.push(json!({ "name": name }));
                }
            }
            json!({
                "intents": intents,
                "wait_matchers": wait_matchers,
                "states": Value::Array(Vec::new()),
            })
        };
        Ok(json!({
            "plugin": entry.manifest.name,
            "manifest": entry.manifest,
            "intents": catalog.get("intents").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
            "wait_matchers": catalog.get("wait_matchers").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
            "states": catalog.get("states").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
        }))
    }

    /// `plugin.load` — register a trusted-local third-party plugin from a
    /// TOML manifest path. Gated by the `allow_plugin_load` server flag,
    /// which operators set with `ptywright serve --allow-plugin-load`.
    /// Without that flag the method returns `-32004 PermissionDenied` with
    /// `data.reason = "server_did_not_grant_plugin_load"` so callers can
    /// distinguish a server-mode denial from an adapter-permission denial.
    fn plugin_load(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        self.require_plugin_load_enabled("plugin.load")?;
        let params: PluginLoadParams = parse_params(params)?;
        let (manifest, source) = PluginManifest::load_from_toml_path(&params.manifest_path)
            .map_err(rpc_error_from_error)?;
        let name = manifest.name.clone();
        self.shared
            .register_plugin(manifest, source)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "plugin": name }))
    }

    /// `plugin.unload` — deregister a previously loaded third-party plugin.
    /// Built-in plugins (claude-code today) cannot be unloaded — they are
    /// embedded in the binary and removing them would break clients that
    /// expect them in the registry. Plugins with live adapters bound to
    /// them are also rejected; callers must `adapter.close` first.
    fn plugin_unload(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        self.require_plugin_load_enabled("plugin.unload")?;
        let params: PluginUnloadParams = parse_params(params)?;
        let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let Some(entry) = shared.registered_plugins.get(&params.plugin).cloned() else {
            return Err((
                RpcErrorCode::InvalidParams,
                format!("unknown plugin: {}", params.plugin),
            )
                .into());
        };
        if entry.builtin {
            return Err((
                RpcErrorCode::InvalidParams,
                format!(
                    "plugin `{}` is built in and cannot be unloaded",
                    params.plugin
                ),
            )
                .into());
        }
        // Refuse to unload while any adapter is still bound to this plugin.
        // Read the sibling adapter_plugin map (adapter id → plugin name,
        // maintained without per-adapter mutex protection) so a long-running
        // `adapter.wait` on any adapter cannot make the bound check race. A
        // previous `try_lock`-based check returned "not bound" on contention
        // and let unload race past the documented "live adapters" guarantee.
        let bound = shared
            .adapter_plugin
            .values()
            .any(|plugin| plugin == &params.plugin);
        if bound {
            return Err((
                RpcErrorCode::InvalidParams,
                format!(
                    "plugin `{}` has live adapters; call adapter.close first",
                    params.plugin
                ),
            )
                .into());
        }
        shared.registered_plugins.remove(&params.plugin);
        // Mirror the unload into the live registry. The Arc<Mutex<LuaPlugin>>
        // returned here is dropped immediately — any in-flight wait
        // holding a clone keeps the instance alive until it completes,
        // so unloading mid-wait is safe even though subsequent waits
        // will fail with "plugin not registered".
        shared.plugin_registry.remove(&params.plugin);
        Ok(json!({ "unloaded": true }))
    }

    /// Common gate for the two `plugin.*` mutation methods. Returns a
    /// `PermissionDenied` payload with a `data.reason` field when the server
    /// was started without `--allow-plugin-load`.
    fn require_plugin_load_enabled(
        &self,
        method: &str,
    ) -> std::result::Result<(), RpcErrorPayload> {
        let allow = self
            .shared
            .inner
            .lock()
            .expect("rpc shared state poisoned")
            .allow_plugin_load;
        if allow {
            return Ok(());
        }
        Err(RpcErrorPayload {
            code: RpcErrorCode::PermissionDenied,
            message: format!(
                "method `{method}` is disabled; restart ptywright serve with --allow-plugin-load"
            ),
            data: Some(json!({
                "method": method,
                "reason": "server_did_not_grant_plugin_load",
            })),
        })
    }

    fn session(&self, id: &str) -> std::result::Result<Arc<Session>, RpcErrorPayload> {
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
                    .into()
            })
    }

    fn allocate_session_id(&mut self) -> String {
        let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let id = format!("s{}", shared.next_session);
        shared.next_session += 1;
        id
    }

    fn allocate_extension_id(&self) -> String {
        let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let id = format!("e{}", shared.next_extension);
        shared.next_extension += 1;
        id
    }

    /// Clone a handle to the named adapter out of shared state. The
    /// caller then locks the returned `Mutex` to actually use the entry.
    fn extension(
        &self,
        id: &str,
    ) -> std::result::Result<Arc<Mutex<ExtensionEntry>>, RpcErrorPayload> {
        let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        shared.extensions.get(id).cloned().ok_or_else(|| {
            (
                RpcErrorCode::InvalidParams,
                format!("unknown adapter: {id}"),
            )
                .into()
        })
    }

    /// Look up the adapter's manifest and verify it declares the permission
    /// required by `method`. Returns a `PermissionDenied` error tuple ready to
    /// hand back from a `*_handler` if the check fails. Methods that have no
    /// permission requirement registered in [`ADAPTER_METHOD_PERMISSIONS`]
    /// short-circuit as allow.
    ///
    /// Permissions are checked at dispatch time so that an adapter's runtime
    /// privileges cannot be widened after `adapter.start` — the manifest the
    /// adapter was started with is the authoritative declaration for the
    /// lifetime of the handle.
    fn check_adapter_permission(
        &self,
        method: &str,
        adapter_id: &str,
    ) -> std::result::Result<(), RpcErrorPayload> {
        let Some(required) = required_permission_for(method) else {
            return Ok(());
        };
        let entry_arc = self.extension(adapter_id)?;
        let entry = entry_arc.lock().expect("extension poisoned");
        if entry
            .handle
            .extension()
            .manifest()
            .permissions
            .contains(&required)
        {
            Ok(())
        } else {
            Err(rpc_error_from_error(Error::PermissionDenied {
                method: method.to_string(),
                required,
            }))
        }
    }

    /// Verify a plugin manifest declares the permission required by `method`.
    /// Used by `adapter.start`, which must check before the
    /// [`ExtensionHandle`] exists in the registry.
    fn check_manifest_permission(
        method: &str,
        manifest: &PluginManifest,
    ) -> std::result::Result<(), RpcErrorPayload> {
        let Some(required) = required_permission_for(method) else {
            return Ok(());
        };
        if manifest.permissions.contains(&required) {
            Ok(())
        } else {
            Err(rpc_error_from_error(Error::PermissionDenied {
                method: method.to_string(),
                required,
            }))
        }
    }

    // ---- adapter.* handlers ---------------------------------------------

    /// `adapter.list` — enumerate every plugin this server can instantiate.
    /// Reads the shared registry so the response includes built-in plugins
    /// and any trusted-local third-party plugins loaded via CLI `--plugin`
    /// flags or the `plugin.load` JSON-RPC method.
    fn adapter_list(&self) -> Value {
        let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let mut plugins: Vec<&PluginManifest> = shared
            .registered_plugins
            .values()
            .map(|entry| &entry.manifest)
            .collect();
        // Deterministic ordering so wire output is stable across runs and
        // does not depend on HashMap iteration order.
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        json!({ "plugins": plugins })
    }

    /// `adapter.start` — spawn a PTY session and wrap it in an
    /// [`ExtensionHandle`] for the requested plugin. Returns the allocated
    /// adapter id plus the initial classified state.
    fn adapter_start(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterStartParams = parse_params(params)?;
        // Look up the plugin in the shared registry so built-ins and
        // trusted-local third-party plugins (loaded via CLI `--plugin` or RPC
        // `plugin.load`) share one code path.
        let registered = {
            let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            shared
                .registered_plugins
                .get(&params.plugin)
                .cloned()
                .ok_or_else(|| -> RpcErrorPayload {
                    (
                        RpcErrorCode::InvalidParams,
                        format!("unknown plugin: {}", params.plugin),
                    )
                        .into()
                })?
        };
        Self::check_manifest_permission("adapter.start", &registered.manifest)?;
        // Borrow the (owned `String`) module sources as `&str` so they
        // match `LuaPlugin::trusted_with_modules`'s slice-of-pairs API
        // without forcing it to take owned strings.
        let module_refs: Vec<(&str, &str)> = registered
            .modules
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();
        let plugin =
            LuaPlugin::trusted_with_modules(&registered.manifest, &registered.source, &module_refs)
                .map_err(rpc_error_from_error)?;
        let extension = LuaExtension::new(plugin, registered.manifest.clone());
        // Fall back to the manifest's declared default target when the caller
        // omits `program`. Args follow the same rule independently: an
        // explicit `args` list always wins, otherwise the manifest default's
        // args are used.
        let manifest_default = extension.manifest().default_target.clone();
        let program = params
            .program
            .or_else(|| manifest_default.as_ref().map(|t| t.program.clone()))
            .ok_or_else(|| -> RpcErrorPayload {
                (
                    RpcErrorCode::InvalidParams,
                    format!(
                        "no host-known default program for plugin `{plugin}`; pass `program` explicitly",
                        plugin = params.plugin,
                    ),
                )
                    .into()
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
        // Geometry resolution order: explicit caller value, then manifest's
        // declared headless preset, then the host's last-resort
        // `rows = 40, cols = 120`. The manifest preset matters for TUI
        // plugins whose classifier depends on line wrapping (claude-code in
        // particular renders status-bar / prompt anchors at column-sensitive
        // positions and ships a `rows = 60, cols = 200` preset for that
        // reason).
        let manifest_rows = manifest_default.as_ref().and_then(|t| t.rows);
        let manifest_cols = manifest_default.as_ref().and_then(|t| t.cols);
        let size = TerminalSize {
            rows: params.rows.or(manifest_rows).unwrap_or(40),
            cols: params.cols.or(manifest_cols).unwrap_or(120),
            pixel_width: params.pixel_width.unwrap_or(0),
            pixel_height: params.pixel_height.unwrap_or(0),
        };
        let mut target = Target::new(program).args(args).size(size);
        target.cwd = params.cwd;
        target.env = merge_env(
            manifest_default.as_ref().map(|t| &t.env),
            params.env,
            manifest_default.as_ref().map(|t| &t.required_env),
        );
        let mut session =
            Session::spawn(SessionConfig::new(target)).map_err(rpc_error_from_error)?;
        // Bind the shared plugin registry so `Matcher::Lua` predicates
        // resolve through `Session::wait_for[_cancellable]`. The
        // registry's plugin instances are SEPARATE from this adapter's
        // `LuaExtension` instance — predicates cannot read classifier
        // module-level state (see `LuaPluginRegistry` docs).
        let registry = {
            let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            Arc::clone(&shared.plugin_registry)
        };
        session.set_plugin_registry(registry);
        let handle = ExtensionHandle::start(
            Box::new(extension),
            session,
            ADAPTER_COMPLETED_TURN_STABLE_MS,
        );
        let state = handle.state();
        let id = self.allocate_extension_id();
        let session_id = self.allocate_session_id();
        {
            let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            shared.extensions.insert(
                id.clone(),
                Arc::new(Mutex::new(ExtensionEntry {
                    plugin: params.plugin.clone(),
                    session: session_id.clone(),
                    handle,
                })),
            );
            shared
                .adapter_plugin
                .insert(id.clone(), params.plugin.clone());
            shared
                .adapter_session
                .insert(id.clone(), session_id.clone());
        }
        Ok(json!({
            "adapter": id,
            "plugin": params.plugin,
            "session": session_id,
            "state": state,
        }))
    }

    /// `adapter.resume` — convenience wrapper around `adapter.start` that
    /// first closes a prior adapter (if supplied and still live), then
    /// spawns a fresh adapter with the same `adapter.start` parameter shape.
    ///
    /// Designed for callers chaining sessions across PTY restarts — typically
    /// re-spawning with a `--resume <uuid>` style flag that the TUI itself
    /// supports — so the consumer doesn't have to make a separate
    /// `adapter.close` call before re-starting. Permission gate is identical
    /// to `adapter.start` (the `prior_adapter` close is best-effort and
    /// silently no-ops on missing ids, so it does not constitute an
    /// additional capability boundary).
    fn adapter_resume(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterResumeParams = parse_params(params)?;
        // Best-effort cleanup of the prior adapter. We ignore the result
        // entirely — a missing id means "already closed / never started",
        // which is the idempotent semantic callers expect. Per-method
        // permission gating ran above the dispatcher so we don't re-check.
        if let Some(prior) = params.prior_adapter.as_deref() {
            let _ = self.adapter_close(Some(json!({ "adapter": prior })));
        }
        let forwarded = json!({
            "plugin": params.plugin,
            "program": params.program,
            "args": params.args,
            "cwd": params.cwd,
            "env": params.env,
            "rows": params.rows,
            "cols": params.cols,
            "pixel_width": params.pixel_width,
            "pixel_height": params.pixel_height,
        });
        self.adapter_start(Some(forwarded))
    }

    /// `adapter.state` — re-classify and return the current state without
    /// applying any actions.
    fn adapter_state(&self, params: Option<Value>) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterParams = parse_params(params)?;
        self.check_adapter_permission("adapter.state", &params.adapter)?;
        let entry_arc = self.extension(&params.adapter)?;
        let entry = entry_arc.lock().expect("extension poisoned");
        Ok(json!({ "state": entry.handle.state() }))
    }

    /// `adapter.send` — invoke a named plugin intent (e.g. `send_prompt`,
    /// `approve`, `deny`, `cancel`) and return the post-apply state. The
    /// intent name is forwarded verbatim to the plugin so the host carries
    /// no application-specific dispatch table.
    fn adapter_send(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterSendParams = parse_params(params)?;
        self.check_adapter_permission("adapter.send", &params.adapter)?;
        let intent = params.intent.clone();
        let entry_arc = self.extension(&params.adapter)?;
        let mut entry = entry_arc.lock().expect("extension poisoned");
        let state = entry
            .handle
            .send(&intent, params.params)
            .map_err(rpc_error_from_error)?;
        Ok(json!({ "state": state }))
    }

    /// `adapter.wait` — block until the plugin's named matcher fires or the
    /// timeout expires, then classify and return the resulting state plus the
    /// structured `matched` outcome describing which matcher branch fired.
    /// The intent defaults to `wait_turn_matcher` so simple callers can omit
    /// it. Response shape: `{ "state": <state>, "matched": <outcome|null> }`.
    /// `matched` is reserved as `null` for future cancellation paths that
    /// surface a `MatchResult` without a satisfying branch; today every
    /// successful wait carries a populated outcome.
    fn adapter_wait(&self, params: Option<Value>) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterWaitParams = parse_params(params)?;
        self.check_adapter_permission("adapter.wait", &params.adapter)?;
        let intent = params
            .intent
            .unwrap_or_else(|| DEFAULT_WAIT_INTENT.to_string());
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(120_000));
        // If the caller supplied `wait_id`, mint a CancellationToken
        // and register it BEFORE we acquire the per-adapter mutex so
        // a same-id cancel arriving from another connection during
        // the brief lock-acquire window is not lost.
        let cancel_token =
            self.register_pending_wait(&params.adapter, params.wait_id.as_deref())?;
        // Cleanup guard: dropping `Cleanup` removes the registry entry
        // regardless of how this function returns (success, timeout,
        // cancel, error). Using a guard avoids duplicating the
        // remove call at every exit path below.
        let _cleanup = PendingWaitCleanup {
            shared: self.shared.clone(),
            wait_id: params.wait_id.clone(),
        };
        let entry_arc = self.extension(&params.adapter)?;
        // Holding the per-adapter mutex across `wait` blocks concurrent
        // sends to the same adapter for the duration of the wait. That is
        // intentional for v1 — wait + send on the same adapter from two
        // clients would be ambiguous anyway. With the cancellation token
        // bound here, `adapter.cancel_wait` from another connection can
        // break out of the wait without contesting the per-adapter mutex.
        let entry = entry_arc.lock().expect("extension poisoned");
        let result = match &cancel_token {
            Some(token) => entry
                .handle
                .wait_with_cancel(&intent, params.params, timeout, token),
            None => entry.handle.wait(&intent, params.params, timeout),
        };
        let (state, outcome) = result.map_err(rpc_error_from_error)?;
        // Always emit `matched` so consumers don't have to branch on key
        // presence. Today every successful wait carries a `Some(outcome)`;
        // `null` is reserved for future paths (e.g. cancellation hooks)
        // that surface a `MatchResult` without a satisfying branch.
        let mut response = json!({
            "state": state,
            "matched": outcome,
        });
        if let Some(id) = params.wait_id {
            response["wait_id"] = json!(id);
        }
        Ok(response)
    }

    /// Register a [`crate::CancellationToken`] in `pending_waits`
    /// when the caller supplied a `wait_id`. Returns the token (which
    /// the wait method then passes to `wait_with_cancel`) or `None` if
    /// the caller didn't request cancellability.
    ///
    /// Duplicate wait_id is rejected with `InvalidParams` — clients
    /// should mint a fresh id (UUIDv4 etc.) per wait. The check is
    /// strict to surface authoring bugs early rather than silently
    /// share a token between two unrelated waits.
    fn register_pending_wait(
        &self,
        adapter: &str,
        wait_id: Option<&str>,
    ) -> std::result::Result<Option<crate::session::CancellationToken>, RpcErrorPayload> {
        let Some(wait_id) = wait_id else {
            return Ok(None);
        };
        let token = crate::session::CancellationToken::new();
        let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        if shared.pending_waits.contains_key(wait_id) {
            return Err((
                RpcErrorCode::InvalidParams,
                format!("wait_id `{wait_id}` is already in flight"),
            )
                .into());
        }
        shared.pending_waits.insert(
            wait_id.to_string(),
            PendingWait {
                adapter: adapter.to_string(),
                token: token.clone(),
            },
        );
        Ok(Some(token))
    }

    fn adapter_cancel_wait(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterCancelWaitParams = parse_params(params)?;
        // Flip the token but DO NOT remove the entry — the originating
        // wait's RAII cleanup guard owns the removal. Removing here
        // would open a reuse race: between this remove and the
        // cancelled wait's cleanup, a new wait could register the same
        // id with a fresh token, and the cleanup guard would then
        // remove the new entry. Read-and-flip leaves the entry in
        // place; the cleanup guard handles the lifecycle when the
        // wait actually unwinds.
        let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
        let Some(entry) = shared.pending_waits.get(&params.wait_id) else {
            // Idempotent: an unknown wait_id (already completed, never
            // existed, or just cleaned up by its own guard) returns
            // `cancelled: false` rather than an error so callers don't
            // have to race the wait to know whether the cancel landed.
            return Ok(json!({ "cancelled": false }));
        };
        let adapter = entry.adapter.clone();
        entry.token.cancel();
        drop(shared);
        Ok(json!({
            "cancelled": true,
            "adapter": adapter,
            "wait_id": params.wait_id,
        }))
    }

    /// `adapter.turn` — atomic `adapter.send` followed by `adapter.wait`
    /// on the same adapter, holding the per-adapter mutex across both
    /// legs so no other connection can slip an intent between them.
    ///
    /// Maps directly to [`ExtensionHandle::turn`]. Requires both
    /// `input.write` and `matcher.wait` permissions (each pinned by the
    /// underlying leg) — neither check is skippable.
    ///
    /// `wait.intent` defaults to `wait_turn_matcher` and the whole `wait`
    /// block may be omitted entirely, which lets simple call sites express
    /// a "submit and wait for the turn to complete" round-trip as one RPC.
    ///
    /// **Head-of-line blocking warning.** The per-adapter mutex is held
    /// for the entire `wait` leg, whose default timeout is 120 s (and
    /// caller-supplied `timeout_ms` can be longer). For the duration of
    /// that wait, every other RPC method that touches the same adapter
    /// (`adapter.state`, `adapter.send`, `adapter.snapshot`,
    /// `adapter.close`, plus `plugin.unload`'s live-adapter check) on
    /// any connection will block on this mutex. That's the intended
    /// atomicity contract for `turn` — it's the property that rules
    /// out a competing intent slipping in — but callers driving a busy
    /// adapter from multiple connections should be aware of it. Use
    /// `adapter.send` + `adapter.wait` separately when you need
    /// finer-grained scheduling.
    fn adapter_turn(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterTurnParams = parse_params(params)?;
        self.check_adapter_permission("adapter.send", &params.adapter)?;
        self.check_adapter_permission("adapter.wait", &params.adapter)?;
        let timeout = Duration::from_millis(params.wait.timeout_ms.unwrap_or(120_000));
        // Same `wait_id` registration + RAII cleanup as `adapter.wait`
        // — only the wait leg participates in cancellation; the send
        // leg is fast and not interruptible.
        let cancel_token =
            self.register_pending_wait(&params.adapter, params.wait.wait_id.as_deref())?;
        let _cleanup = PendingWaitCleanup {
            shared: self.shared.clone(),
            wait_id: params.wait.wait_id.clone(),
        };
        let entry_arc = self.extension(&params.adapter)?;
        let mut entry = entry_arc.lock().expect("extension poisoned");
        let result = match &cancel_token {
            Some(token) => entry.handle.turn_with_cancel(
                &params.send.intent,
                params.send.params,
                params.wait.intent.as_deref(),
                params.wait.params,
                timeout,
                token,
            ),
            None => entry.handle.turn(
                &params.send.intent,
                params.send.params,
                params.wait.intent.as_deref(),
                params.wait.params,
                timeout,
            ),
        };
        let (state, outcome) = result.map_err(rpc_error_from_error)?;
        let mut response = json!({
            "state": state,
            "matched": outcome,
        });
        if let Some(id) = params.wait.wait_id {
            response["wait_id"] = json!(id);
        }
        Ok(response)
    }

    /// `adapter.snapshot` — passthrough to the adapter's underlying session
    /// snapshot. Mirrors `session.snapshot`'s redaction semantics.
    fn adapter_snapshot(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterReadParams = parse_params(params)?;
        self.check_adapter_permission("adapter.snapshot", &params.adapter)?;
        let entry_arc = self.extension(&params.adapter)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let entry = entry_arc.lock().expect("extension poisoned");
        let mut snapshot = entry.handle.session().snapshot();
        if let Some(policy) = policy {
            snapshot = snapshot.redacted(&policy);
        }
        serde_json::to_value(snapshot)
            .map_err(|error| (RpcErrorCode::InternalError, error.to_string()).into())
    }

    /// `adapter.transcript` — passthrough to the adapter's underlying session
    /// transcript. Mirrors `session.transcript`'s redaction semantics.
    fn adapter_transcript(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterReadParams = parse_params(params)?;
        self.check_adapter_permission("adapter.transcript", &params.adapter)?;
        let entry_arc = self.extension(&params.adapter)?;
        let policy = if params.redact.unwrap_or(true) {
            Some(redaction_policy_for_read(params.redaction)?)
        } else {
            None
        };
        let entry = entry_arc.lock().expect("extension poisoned");
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
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterReadParams = parse_params(params)?;
        self.check_adapter_permission("adapter.inspect", &params.adapter)?;
        let entry_arc = self.extension(&params.adapter)?;
        let entry = entry_arc.lock().expect("extension poisoned");
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
    ) -> std::result::Result<Value, RpcErrorPayload> {
        let params: AdapterParams = parse_params(params)?;
        self.check_adapter_permission("adapter.close", &params.adapter)?;
        let removed = {
            let mut shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            // Remove from the live-handle registry plus both sibling
            // maps atomically under the same lock so plugin.unload and
            // the notification filter never see a half-removed adapter.
            shared.adapter_plugin.remove(&params.adapter);
            shared.adapter_session.remove(&params.adapter);
            shared.extensions.remove(&params.adapter)
        };
        if let Some(entry_arc) = removed {
            let entry = entry_arc.lock().expect("extension poisoned");
            // Best-effort kill — if the child already exited the kill returns
            // an error which we discard. The session is dropped immediately
            // afterwards either way.
            let _ = entry.handle.session().kill();
            // Adapter sessions share the `s<n>` id namespace with directly-
            // created sessions, and the notification trackers are keyed on
            // that id. Drop the per-connection state so long-lived REPL
            // sessions that spawn and close many adapters don't accumulate
            // entries forever.
            let session_id = entry.session.clone();
            drop(entry);
            self.forget_session_notification_state(&session_id);
            Ok(json!({ "closed": true }))
        } else {
            Err((
                RpcErrorCode::InvalidParams,
                format!("unknown adapter: {}", params.adapter),
            )
                .into())
        }
    }

    /// Drop all per-connection notification tracking state for `session_id`.
    /// Called by both `session.close` and `adapter.close` so the three
    /// notification maps stay in lock-step regardless of how a session was
    /// allocated. Idempotent: missing keys are silently ignored.
    fn forget_session_notification_state(&mut self, session_id: &str) {
        self.last_notified_sequences.remove(session_id);
        self.last_notified_outputs.remove(session_id);
        self.notified_exits.remove(session_id);
    }

    /// `adapter.live` — list every adapter currently registered in shared
    /// state. Useful for re-connecting clients that need to discover
    /// adapters left running by a previous REPL session.
    fn adapter_live(&self) -> Value {
        let snapshot: Vec<Value> = {
            let shared = self.shared.inner.lock().expect("rpc shared state poisoned");
            let mut entries: Vec<(String, Arc<Mutex<ExtensionEntry>>)> = shared
                .extensions
                .iter()
                .map(|(id, arc)| (id.clone(), Arc::clone(arc)))
                .collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            entries
        }
        .into_iter()
        .map(|(id, arc)| {
            let entry = arc.lock().expect("extension poisoned");
            let session = entry.handle.session();
            json!({
                "adapter": id,
                "plugin": entry.plugin,
                "session": entry.session,
                "sequence": session.sequence(),
                "finished": session.is_finished(),
            })
        })
        .collect();
        json!({ "adapters": snapshot })
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

/// Merge a plugin manifest's env with caller-supplied `env` from
/// `adapter.start`.
///
/// Precedence (lowest → highest):
///
/// 1. Manifest `default_target.env` — plugin-recommended defaults.
/// 2. Caller `env` — overrides manifest defaults on key conflict.
/// 3. Manifest `default_target.required_env` — plugin-mandated keys
///    the caller cannot override.
///
/// A missing manifest map is treated as empty. The required-env tier
/// makes safety-critical knobs (terminal-title disabling,
/// virtual-scroll suppression) stable across all callers.
fn merge_env(
    manifest_default: Option<&BTreeMap<String, String>>,
    caller: BTreeMap<String, String>,
    manifest_required: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut merged = manifest_default.cloned().unwrap_or_default();
    merged.extend(caller);
    if let Some(required) = manifest_required {
        merged.extend(required.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    merged
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

fn parse_params<T>(params: Option<Value>) -> std::result::Result<T, RpcErrorPayload>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| (RpcErrorCode::InvalidParams, error.to_string()).into())
}

fn redaction_policy_for_read(
    policy: Option<RedactionPolicy>,
) -> std::result::Result<RedactionPolicy, RpcErrorPayload> {
    let mut policy = policy.unwrap_or_default();
    policy.enabled = true;
    policy
        .validate()
        .map_err(|error| -> RpcErrorPayload { (RpcErrorCode::InvalidParams, error).into() })?;
    Ok(policy)
}

fn success_response(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn error_response(id: Option<Value>, payload: RpcErrorPayload) -> Value {
    let message = RedactionPolicy::default().redact(&payload.message);
    let mut error = json!({
        "code": payload.code.code(),
        "message": message,
    });
    // Only surface `data` on the wire when the dispatcher actually attached
    // a structured payload. Most errors do not.
    if let Some(data) = payload.data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "error": error,
    })
}

fn serialize_response(value: Value) -> Result<String> {
    Ok(serde_json::to_string(&value)?)
}

fn rpc_error_from_error(error: Error) -> RpcErrorPayload {
    let message = error.to_string();
    let (code, data) = match &error {
        Error::Timeout => (RpcErrorCode::Timeout, None),
        Error::Closed | Error::ReaderEnded => (RpcErrorCode::SessionClosed, None),
        Error::PermissionDenied { method, required } => (
            RpcErrorCode::PermissionDenied,
            Some(json!({
                "method": method,
                "required_permission": required.as_str(),
            })),
        ),
        Error::Cancelled => (RpcErrorCode::Cancelled, None),
        Error::Pty(_)
        | Error::Io(_)
        | Error::Json(_)
        | Error::Lua(_)
        | Error::Rpc(_)
        | Error::Config(_)
        | Error::UnsupportedOnPlatform(_) => (RpcErrorCode::InternalError, None),
    };
    RpcErrorPayload {
        code,
        message,
        data,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::plugin::claude_code_manifest;

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
    #[cfg(unix)]
    fn adapters_survive_disconnect_and_are_visible_to_other_handlers() {
        // Regression for "exiting the REPL kills the adapter": once an
        // `adapter.start` succeeds, dropping the spawning RpcServer must
        // *not* drop the adapter. A second RpcServer over the same shared
        // state can still send / read / inspect / close it.
        let state = RpcServerState::new();
        let adapter;
        {
            let mut first = RpcServer::with_state(state.clone());
            let start = handle(
                &mut first,
                r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'survive-disconnect\\n' && cat"]}}"#,
            );
            adapter = start["result"]["adapter"]
                .as_str()
                .expect("adapter id")
                .to_string();
            // `first` drops here — without persistent adapters this would
            // tear down the PTY child.
        }

        let mut second = RpcServer::with_state(state);
        let live = handle(
            &mut second,
            r#"{"jsonrpc":"2.0","id":2,"method":"adapter.live"}"#,
        );
        let live_adapters = live["result"]["adapters"].as_array().expect("array");
        assert!(
            live_adapters
                .iter()
                .any(|row| row["adapter"] == adapter.as_str()),
            "adapter.live must surface adapters spawned by a previous handler; got {live_adapters:?}"
        );

        // A read against the original adapter id must still succeed.
        let state_call = handle(
            &mut second,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.state","params":{{"adapter":"{adapter}"}}}}"#,
            ),
        );
        assert!(
            state_call["result"]["state"].is_object(),
            "adapter.state must still work after the spawning handler is dropped; got {state_call}"
        );

        // Clean up.
        let _ = handle(
            &mut second,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#,
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn notification_filter_scopes_to_named_sessions_only() {
        // With `server.set_notifications {enabled, sessions:["s1"]}`, only
        // s1's events should surface; s2 stays silent even though both
        // are producing output.
        let mut server = RpcServer::new();
        let create1 = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","sleep 5"]}}"#,
            )
            .expect("create s1");
        let s1: Value = serde_json::from_str(&create1[0]).unwrap();
        let s1_id = s1["result"]["session"].as_str().unwrap().to_string();
        let create2 = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":2,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","printf hello; sleep 5"]}}"#,
            )
            .expect("create s2");
        let s2: Value = serde_json::from_str(&create2[0]).unwrap();
        let s2_id = s2["result"]["session"].as_str().unwrap().to_string();

        let _ = server
            .handle_line_messages(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"server.set_notifications","params":{{"enabled":true,"sessions":["{s1_id}"]}}}}"#,
            ))
            .expect("enable filtered notifications");

        let poll = server
            .handle_line_messages(&format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"session.wait","params":{{"session":"{s2_id}","matcher":{{"type":"contains_text","value":"hello"}},"timeout_ms":3000}}}}"#,
            ))
            .expect("wait for s2");

        let stitched = poll.join("\n");
        assert!(
            !stitched.contains(&format!("\"session\":\"{s2_id}\""))
                || !stitched.contains("session.changed"),
            "s2 changes must not surface when filter scopes to s1; got: {stitched}"
        );

        // Cleanup.
        let _ = server.handle_line_messages(&format!(
            r#"{{"jsonrpc":"2.0","id":98,"method":"session.kill","params":{{"session":"{s1_id}"}}}}"#,
        ));
        let _ = server.handle_line_messages(&format!(
            r#"{{"jsonrpc":"2.0","id":99,"method":"session.kill","params":{{"session":"{s2_id}"}}}}"#,
        ));
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

        // The `adapter.start` handler runs `poll_notifications` itself
        // before returning, so if the spawned shell already wrote
        // `adapter-notify` to the PTY by the time `handle_line_messages`
        // unwinds (which is what happens under Ubuntu CI's parallel-test
        // load) the resulting `session.changed` lands inside
        // `start_messages` — not in any subsequent poll. The polling loop
        // below cannot wake itself either, because the test shell drops
        // into `cat` and emits no further bytes. Check both buckets so
        // either ordering is accepted.
        let saw_in = |messages: &[String]| {
            messages.iter().any(|message| {
                message.contains("\"method\":\"session.changed\"") && message.contains(&session)
            })
        };
        let mut saw_changed = saw_in(&start_messages);

        // Generous 20 s deadline keeps the loop branch reliable on busy
        // CI hosts. The steady-state behaviour is observed within tens of
        // milliseconds locally; we only need this much budget when many
        // parallel `/bin/sh` PTY spawns + mlua's send-mutex add overhead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline && !saw_changed {
            let poll_messages = server
                .handle_line_messages(&format!(
                    r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.state","params":{{"adapter":"{adapter}"}}}}"#,
                ))
                .expect("poll for notifications");
            saw_changed = saw_in(&poll_messages);
            if !saw_changed {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        assert!(
            saw_changed,
            "expected session.changed notification for adapter session `{session}` within 20s"
        );

        // Drain and close.
        let _ = server.handle_line_messages(&format!(
            r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#,
        ));
    }

    /// Regression for the contention bug Copilot called out on
    /// `poll_notifications`: `adapter.wait` holds the per-entry mutex for
    /// the entire wait duration. If `poll_notifications` used `.lock()`
    /// (rather than `try_lock`) to read each entry's `session().sequence()`,
    /// a long wait on adapter A would block notification polling for
    /// adapter B too — silencing the heartbeat-driven flush every other
    /// REPL relies on.
    ///
    /// Simulate the in-flight wait by manually holding adapter A's
    /// per-entry mutex on the test thread, then assert that a `session.changed`
    /// notification still surfaces for adapter B. With the bug present
    /// the test deadlocks; with `try_lock` we skip A this tick and B
    /// flows through normally.
    #[test]
    #[cfg(unix)]
    fn poll_notifications_skips_busy_adapter_entries() {
        let mut server = RpcServer::new();
        let _ = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
            )
            .expect("enable notifications");

        // Two adapters, each writing a single line then dropping into
        // `cat` so the session sequence advances exactly once on each.
        let start_a = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":2,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'a\\n' && cat"]}}"#,
            )
            .expect("start adapter A");
        let resp_a: Value = serde_json::from_str(&start_a[0]).expect("json response A");
        let adapter_a = resp_a["result"]["adapter"]
            .as_str()
            .expect("adapter id A")
            .to_string();

        let start_b = server
            .handle_line_messages(
                r#"{"jsonrpc":"2.0","id":3,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'b\\n' && cat"]}}"#,
            )
            .expect("start adapter B");
        let resp_b: Value = serde_json::from_str(&start_b[0]).expect("json response B");
        let session_b = resp_b["result"]["session"]
            .as_str()
            .expect("session id B")
            .to_string();

        // Helper: scan a batch of JSON-RPC messages for a
        // `session.changed` notification mentioning adapter B's session
        // id. Same shape as the sister test
        // `notifications_report_adapter_session_changes` uses.
        let saw_in = |messages: &[String]| {
            messages.iter().any(|message| {
                message.contains("\"method\":\"session.changed\"") && message.contains(&session_b)
            })
        };
        // The B-start handler runs `poll_notifications` itself before
        // returning. Under llvm-cov instrumentation (slow PTY spawn)
        // the printf can land before that pass executes, so the very
        // first `session.changed` for B may already be in `start_b`
        // rather than any subsequent poll. Check the start batch as
        // the seed for `saw_b`.
        let mut saw_b = saw_in(&start_b);

        // Hold adapter A's per-entry mutex from this thread to mimic
        // an in-flight `adapter.wait`. Acquire the Arc first under a
        // brief outer-state lock; drop the outer guard before locking
        // the inner mutex so we don't block other shared-state lookups.
        let arc_a = {
            let inner = server.shared.inner.lock().expect("shared state");
            inner
                .extensions
                .get(&adapter_a)
                .expect("adapter A entry")
                .clone()
        };
        let _held = arc_a.lock().expect("hold adapter A mutex");

        // Drive the notification pump via the cheapest read-only method
        // — `server.capabilities` is what the REPL's heartbeat uses for
        // exactly this reason. With the bug present the call deadlocks
        // here forever (waiting on adapter A's mutex). With `try_lock`
        // the contended A entry is skipped and B's notification flows
        // through normally.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !saw_b {
            let messages = server
                .handle_line_messages(r#"{"jsonrpc":"2.0","id":99,"method":"server.capabilities"}"#)
                .expect("poll via capabilities");
            saw_b = saw_in(&messages);
            if !saw_b {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        assert!(
            saw_b,
            "expected session.changed for adapter B while adapter A's entry mutex is held"
        );

        drop(_held);
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
    fn adapter_resume_closes_prior_adapter_and_spawns_fresh() {
        // adapter.resume is a thin convenience: close the prior adapter
        // (best-effort, idempotent) then spawn a new one with the same
        // adapter.start shape. The two adapter IDs must differ, and the
        // prior id must no longer be in the registry after the resume.
        let mut server = RpcServer::new();
        let first = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-c","sleep 5"]}}"#,
        );
        let first_adapter = first["result"]["adapter"]
            .as_str()
            .expect("first adapter id")
            .to_string();

        let resumed = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.resume","params":{{"plugin":"claude-code","program":"/bin/sh","args":["-c","sleep 5"],"prior_adapter":"{first_adapter}"}}}}"#
            ),
        );
        let new_adapter = resumed["result"]["adapter"]
            .as_str()
            .expect("resumed adapter id")
            .to_string();
        assert_ne!(
            first_adapter, new_adapter,
            "resume must allocate a fresh id"
        );

        // Prior id must no longer be live — subsequent adapter.state
        // against it returns InvalidParams.
        let after = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.state","params":{{"adapter":"{first_adapter}"}}}}"#
            ),
        );
        assert_eq!(after["error"]["code"], -32602);

        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{new_adapter}"}}}}"#
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn adapter_resume_without_prior_adapter_just_spawns() {
        // Omitting prior_adapter must still work — resume is then literally
        // adapter.start with a different method name.
        let mut server = RpcServer::new();
        let resumed = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.resume","params":{"plugin":"claude-code","program":"/bin/sh","args":["-c","sleep 5"]}}"#,
        );
        let adapter = resumed["result"]["adapter"]
            .as_str()
            .expect("resume without prior must spawn");
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn adapter_resume_silently_ignores_missing_prior_adapter() {
        // Idempotent: a prior_adapter that doesn't exist must not break
        // the resume — callers can retry resume after a crash without
        // first checking adapter.live.
        let mut server = RpcServer::new();
        let resumed = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.resume","params":{"plugin":"claude-code","program":"/bin/sh","args":["-c","sleep 5"],"prior_adapter":"never-existed"}}"#,
        );
        let adapter = resumed["result"]["adapter"]
            .as_str()
            .expect("resume must succeed despite missing prior_adapter");
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
    }

    #[test]
    fn capabilities_list_includes_adapter_resume() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}"#,
        );
        let methods = response["result"]["methods"]
            .as_array()
            .expect("methods array");
        assert!(
            methods.iter().any(|m| m == "adapter.resume"),
            "capabilities must advertise adapter.resume; got {methods:?}"
        );
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
    fn merge_env_overlays_caller_on_manifest_defaults() {
        // Manifest defaults form the base; caller env overlays. Three
        // properties to lock:
        //   * keys present only in the manifest survive (inherited)
        //   * keys present only in the caller appear in the merged map
        //   * keys present in both take the caller's value (caller wins)
        let manifest: BTreeMap<String, String> =
            [("MANIFEST_ONLY", "kept"), ("SHARED", "manifest_value")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
        let caller: BTreeMap<String, String> =
            [("CALLER_ONLY", "added"), ("SHARED", "caller_value")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();

        let merged = merge_env(Some(&manifest), caller, None);

        assert_eq!(
            merged.get("MANIFEST_ONLY").map(String::as_str),
            Some("kept"),
            "manifest-only keys must be inherited"
        );
        assert_eq!(
            merged.get("CALLER_ONLY").map(String::as_str),
            Some("added"),
            "caller-only keys must appear in merged map"
        );
        assert_eq!(
            merged.get("SHARED").map(String::as_str),
            Some("caller_value"),
            "caller value must override manifest default on key conflict"
        );
        assert_eq!(
            merged.len(),
            3,
            "merged map must contain exactly the union of keys"
        );
    }

    #[test]
    fn merge_env_with_no_manifest_default_returns_caller_env() {
        let caller: BTreeMap<String, String> = [("A", "v")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let merged = merge_env(None, caller.clone(), None);
        assert_eq!(
            merged, caller,
            "missing manifest default must passthrough caller env"
        );
    }

    #[test]
    fn merge_env_with_empty_caller_returns_manifest_default_clone() {
        let manifest: BTreeMap<String, String> = [("A", "v")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let merged = merge_env(Some(&manifest), BTreeMap::new(), None);
        assert_eq!(
            merged, manifest,
            "empty caller env must yield the manifest defaults verbatim"
        );
    }

    #[test]
    fn merge_env_required_overrides_caller() {
        // Required-env tier: the plugin manifest reserves keys the
        // caller cannot override. This is the load-bearing safety
        // property for settings like terminal-title disabling — if the
        // caller could turn them back on, the classifier would break.
        let defaults: BTreeMap<String, String> = [("DEFAULT", "d")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let caller: BTreeMap<String, String> = [
            ("DEFAULT", "caller_override_default"),
            ("REQUIRED", "caller_attempt"),
            ("CALLER_ONLY", "added"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let required: BTreeMap<String, String> = [("REQUIRED", "plugin_wins")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();

        let merged = merge_env(Some(&defaults), caller, Some(&required));

        assert_eq!(
            merged.get("DEFAULT").map(String::as_str),
            Some("caller_override_default"),
            "caller still overrides manifest defaults for non-required keys"
        );
        assert_eq!(
            merged.get("REQUIRED").map(String::as_str),
            Some("plugin_wins"),
            "required_env wins over caller-supplied value"
        );
        assert_eq!(
            merged.get("CALLER_ONLY").map(String::as_str),
            Some("added"),
            "caller-only keys still pass through"
        );
    }

    #[test]
    #[cfg(unix)]
    fn adapter_start_merges_caller_env_over_manifest_default_env() {
        // The env-resolution path overlays caller-supplied env onto the
        // manifest's `default_target.env` (manifest first, caller wins
        // on conflict, keys the caller omits are inherited). The
        // claude-code manifest ships a non-empty env preset, so passing
        // a non-empty `env` on adapter.start exercises both branches —
        // the manifest-default copy loop and the caller-override loop.
        // Behavioural verification (the spawned child actually receives
        // the merged env) is out of scope here because ptywright needs
        // a PTY-attached child and there's no portable PTY-side env
        // echo in the test toolbox; the lower-level merge contract is
        // covered by Target's existing env unit tests. This test locks
        // the dispatcher path and the merge order documented in the
        // adjacent comment block.
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{
                "plugin":"claude-code",
                "program":"/bin/sh",
                "args":["-c","sleep 5"],
                "env":{
                    "PTYWRIGHT_TEST_CALLER_ONLY":"caller",
                    "CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"caller-overrides-manifest"
                }
            }}"#,
        );
        let adapter = start["result"]["adapter"]
            .as_str()
            .expect("adapter.start with caller env must succeed");
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

        // `adapter.start` now resolves plugins from the shared registry, so a
        // missing name is a caller error (InvalidParams) rather than an
        // internal lookup failure.
        assert_eq!(response["error"]["code"], -32602);
        let message = response["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("unknown plugin: does-not-exist"),
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

        // Structured matcher result: claude-code's wait_turn_matcher is a
        // top-level All([Any([...]), ScreenStable]). On the "Total cost:"
        // anchor the inner Any must surface that branch's contains_text
        // payload, so callers can reason about which boundary anchor fired
        // without re-scanning the screen.
        let matched = &response["result"]["matched"];
        assert_eq!(
            matched["kind"], "all",
            "adapter.wait must surface the structured outcome; got {response}"
        );
        let all_branches = matched["matched"].as_array().expect("all branches array");
        assert!(
            !all_branches.is_empty(),
            "All outcome must carry per-branch detail; got {matched}"
        );
        let any_branch = all_branches
            .iter()
            .find(|branch| branch["kind"] == "any")
            .expect("All must include the Any anchor branch");
        assert_eq!(
            any_branch["matched"]["kind"], "contains_text",
            "Any branch must record which alternative fired; got {any_branch}"
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
    #[cfg(unix)]
    fn adapter_turn_chains_send_and_wait_in_one_round_trip() {
        // adapter.turn sends an intent and waits for the matcher in a
        // single RPC call, holding the per-adapter mutex across both
        // legs. The fixture below prints the `Total cost:` turn-boundary
        // anchor after a tiny delay, which is enough for the
        // wait_turn_matcher anchor list to fire.
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","printf 'Total cost: 0\\n'; sleep 5"]}}"#,
        );
        let adapter = start["result"]["adapter"]
            .as_str()
            .expect("adapter.start must return an adapter id");

        let response = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.turn","params":{{"adapter":"{adapter}","send":{{"intent":"approve"}},"wait":{{"timeout_ms":3000}}}}}}"#
            ),
        );
        assert!(
            response["result"]["state"].is_object(),
            "adapter.turn must return a state snapshot; got {response}"
        );
        assert_eq!(
            response["result"]["matched"]["kind"], "all",
            "adapter.turn must surface the same structured outcome as adapter.wait; got {response}"
        );

        // Cleanup.
        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
    }

    #[test]
    fn adapter_cancel_wait_unknown_id_returns_cancelled_false() {
        // Idempotent: cancelling a wait_id that has no in-flight wait
        // (already completed, never existed, or just expired) must
        // return `cancelled: false` rather than an error so callers
        // don't race the wait to know whether the cancel landed.
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.cancel_wait","params":{"wait_id":"never-was-registered"}}"#,
        );
        assert_eq!(response["result"]["cancelled"], false);
    }

    #[test]
    #[cfg(unix)]
    fn adapter_cancel_wait_interrupts_in_flight_wait_from_another_connection() {
        // The whole point of wait_id-keyed cancellation: a wait blocked
        // on a long timeout in one connection's call stack can be
        // interrupted by a cancel_wait from a SEPARATE connection
        // (here two `RpcServer` instances sharing one `RpcServerState`,
        // mirroring how a Unix socket listener gives each client its
        // own server handler over the same registry). The originating
        // wait must return promptly with the RPC-level Cancelled error
        // (-32005, distinct from -32001 Timeout) — `wait_for_cancellable`
        // surfaces `Error::Cancelled` which maps to the dedicated
        // `RpcErrorCode::Cancelled` wire code.
        use std::thread;

        let state = RpcServerState::default();
        let mut server = RpcServer::with_state(state.clone());

        let start_response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","sleep 30"]}}"#,
        );
        let adapter = start_response["result"]["adapter"]
            .as_str()
            .expect("adapter id present")
            .to_string();

        // Wait on its own `RpcServer` — the wait holds `&mut self` for
        // its full duration, exactly like a real connection would.
        let wait_state = state.clone();
        let wait_adapter = adapter.clone();
        let wait_thread = thread::spawn(move || {
            let mut wait_server = RpcServer::with_state(wait_state);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.wait","params":{{"adapter":"{wait_adapter}","intent":"wait_turn_matcher","timeout_ms":30000,"wait_id":"cancel-test-1"}}}}"#
            );
            handle(&mut wait_server, &request)
        });

        // Give the wait thread a moment to register itself in
        // pending_waits before we issue the cancel.
        thread::sleep(Duration::from_millis(200));

        // Cancel from a third (still-separate) connection.
        let mut cancel_server = RpcServer::with_state(state.clone());
        let cancel_response = handle(
            &mut cancel_server,
            r#"{"jsonrpc":"2.0","id":3,"method":"adapter.cancel_wait","params":{"wait_id":"cancel-test-1"}}"#,
        );
        assert_eq!(
            cancel_response["result"]["cancelled"], true,
            "cancel must find the pending wait; got {cancel_response}"
        );
        assert_eq!(cancel_response["result"]["adapter"], adapter);

        let wait_response = wait_thread.join().expect("wait thread panic");
        let error_code = wait_response["error"]["code"]
            .as_i64()
            .expect("cancelled wait must return a JSON-RPC error code");
        assert_eq!(
            error_code, -32005,
            "cancelled wait must surface as Cancelled (-32005); got {wait_response}"
        );

        let _ = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":99,"method":"adapter.close","params":{{"adapter":"{adapter}"}}}}"#
            ),
        );
    }

    #[test]
    fn register_pending_wait_rejects_duplicate_wait_id() {
        // The per-adapter mutex held by `adapter.wait` makes a
        // duplicate-id scenario hard to reproduce end-to-end (the
        // second wait blocks on the entry lock until the first
        // releases, by which point cleanup has removed the entry).
        // Test `register_pending_wait` directly so the guarantee is
        // covered: the helper itself rejects the second call.
        let state = RpcServerState::default();
        let server = RpcServer::with_state(state);

        let token = server
            .register_pending_wait("adapter-1", Some("dup-id"))
            .expect("first registration succeeds");
        assert!(token.is_some(), "wait_id should mint a token");

        let err = server
            .register_pending_wait("adapter-2", Some("dup-id"))
            .expect_err("duplicate wait_id must error");
        assert_eq!(
            err.code,
            RpcErrorCode::InvalidParams,
            "duplicate must surface as InvalidParams"
        );
        assert!(
            err.message.contains("dup-id"),
            "error message should name the duplicate id; got: {}",
            err.message
        );
    }

    #[test]
    fn register_pending_wait_returns_none_when_wait_id_omitted() {
        let state = RpcServerState::default();
        let server = RpcServer::with_state(state);
        let token = server
            .register_pending_wait("adapter-x", None)
            .expect("no wait_id is a no-op");
        assert!(token.is_none(), "omitted wait_id must not allocate a token");
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
    fn plugin_describe_lists_builtin_plugin_catalog_shape() {
        // Plugins can either provide a `describe()` function in their
        // Lua source or rely on the host's introspection fallback. The
        // built-in plugin opts in, so core only asserts the generic wire
        // shape here. Plugin-specific vocabulary is covered by plugin tests.
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"plugin.describe","params":{"plugin":"claude-code"}}"#,
        );
        let result = &response["result"];
        assert_eq!(result["plugin"], "claude-code");
        assert_eq!(result["manifest"]["name"], "claude-code");

        let intents = result["intents"].as_array().expect("intents array");
        assert!(
            !intents.is_empty(),
            "built-in catalog should expose intents"
        );
        assert!(
            intents.iter().all(|i| i["name"].is_string()),
            "every intent entry should expose a string name: {intents:?}"
        );

        let matchers = result["wait_matchers"]
            .as_array()
            .expect("wait_matchers array");
        assert!(
            !matchers.is_empty(),
            "built-in catalog should expose wait matchers"
        );
        assert!(
            matchers.iter().all(|i| i["name"].is_string()),
            "every wait matcher entry should expose a string name: {matchers:?}"
        );

        let states = result["states"].as_array().expect("states array");
        assert!(!states.is_empty(), "built-in catalog should expose states");
        assert!(
            states.iter().all(|i| i["name"].is_string()),
            "every state entry should expose a string name: {states:?}"
        );
    }

    #[test]
    fn plugin_describe_rejects_unknown_plugin() {
        let mut server = RpcServer::new();
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"plugin.describe","params":{"plugin":"does-not-exist"}}"#,
        );
        assert_eq!(
            response["error"]["code"], -32602,
            "missing plugin must be an InvalidParams error: {response}"
        );
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

    // ---- Permission gating (GH #18) -------------------------------------

    /// Lookup table is the single source of truth; assert each gated method
    /// maps to the permission documented in the v0.1.0 release plan so an
    /// accidental table edit shows up as a test failure with a useful name.
    #[test]
    fn required_permission_for_known_methods() {
        assert_eq!(
            required_permission_for("adapter.start"),
            Some(PluginPermission::SessionSpawn)
        );
        assert_eq!(
            required_permission_for("adapter.send"),
            Some(PluginPermission::InputWrite)
        );
        assert_eq!(
            required_permission_for("adapter.wait"),
            Some(PluginPermission::MatcherWait)
        );
        assert_eq!(
            required_permission_for("adapter.snapshot"),
            Some(PluginPermission::ScreenRead)
        );
        assert_eq!(
            required_permission_for("adapter.transcript"),
            Some(PluginPermission::TranscriptRead)
        );
        assert_eq!(
            required_permission_for("adapter.inspect"),
            Some(PluginPermission::ScreenRead)
        );
        assert_eq!(
            required_permission_for("adapter.state"),
            Some(PluginPermission::ScreenRead)
        );
        assert_eq!(
            required_permission_for("adapter.close"),
            Some(PluginPermission::SessionKill)
        );
        // Registry-query methods are allow-by-default and must stay out of
        // the table — adding them silently would block every connection
        // from listing plugins. Treat their absence as a load-bearing
        // invariant.
        assert_eq!(required_permission_for("adapter.list"), None);
        assert_eq!(required_permission_for("adapter.live"), None);
        assert_eq!(required_permission_for("session.create"), None);
        assert_eq!(required_permission_for("server.capabilities"), None);
    }

    /// `check_manifest_permission` is the path `adapter.start` uses before a
    /// handle exists in the registry. Exercise both branches.
    #[test]
    fn check_manifest_permission_allows_when_declared() {
        let mut manifest = claude_code_manifest();
        manifest.permissions = vec![PluginPermission::SessionSpawn];
        RpcServer::check_manifest_permission("adapter.start", &manifest)
            .expect("manifest declaring required permission must pass");
    }

    #[test]
    fn check_manifest_permission_denies_when_missing() {
        let mut manifest = claude_code_manifest();
        manifest.permissions.clear();
        let err = RpcServer::check_manifest_permission("adapter.start", &manifest)
            .expect_err("manifest missing the required permission must deny");
        assert_eq!(err.code, RpcErrorCode::PermissionDenied);
        assert!(
            err.message.contains("session.spawn"),
            "deny message should name the missing permission: {}",
            err.message
        );
        let data = err
            .data
            .as_ref()
            .expect("permission denied must carry data");
        assert_eq!(data["method"], "adapter.start");
        assert_eq!(data["required_permission"], "session.spawn");
    }

    #[test]
    fn check_manifest_permission_passes_unregistered_methods() {
        // `adapter.list` is not in the table — any manifest, including one
        // with zero permissions, should be allowed through.
        let mut manifest = claude_code_manifest();
        manifest.permissions.clear();
        RpcServer::check_manifest_permission("adapter.list", &manifest)
            .expect("unregistered methods must short-circuit allow");
    }

    /// End-to-end deny path through the dispatcher: build an adapter with a
    /// custom manifest that omits `InputWrite`, inject it directly into shared
    /// state, then call `adapter.send` over the wire and assert the JSON-RPC
    /// error code is `-32004` with the expected message format.
    // `cfg(unix)` because the test spawns `/bin/sh` to back the stub
    // adapter's PTY (we never actually drive that shell — the dispatcher
    // denies the call before it could matter — but the spawn has to
    // succeed for the test to build the registry entry). Windows lacks
    // `/bin/sh`; rather than carry a portable stub binary just for this
    // assertion, restrict the test to Unix and let the
    // `check_manifest_permission_denies_when_missing` unit test (which
    // does not spawn a session) cover the same denial logic on Windows.
    #[test]
    #[cfg(unix)]
    fn adapter_send_denied_when_manifest_lacks_input_write() {
        // Stub Lua source: just enough to satisfy ExtensionHandle::start's
        // initial classify call. We never reach the plugin's send_prompt
        // because the dispatcher should reject the call first.
        let stub_source = r#"
            return {
              classify = function(_ctx)
                return { state = "ready", confidence = 1.0, evidence = "stub" }
              end,
              send_prompt = function(_input)
                return { actions = {}, last_intent = "prompt_submitted" }
              end,
            }
        "#;
        let mut manifest = claude_code_manifest();
        manifest.name = "stub-no-input".to_string();
        // Omit InputWrite; keep everything else so initial state classify
        // succeeds and we can prove the dispatcher denies, not the plugin
        // runtime.
        manifest.permissions = vec![
            PluginPermission::SessionSpawn,
            PluginPermission::ScreenRead,
            PluginPermission::TranscriptRead,
            PluginPermission::MatcherWait,
        ];
        let plugin = crate::lua_plugin::LuaPlugin::trusted(&manifest, stub_source)
            .expect("stub plugin compiles");
        let extension = LuaExtension::new(plugin, manifest);
        let mut session_config = SessionConfig::new(Target::new("/bin/sh").args(["-lc", "cat"]));
        session_config.transcript.max_chars = 1024;
        let session = Session::spawn(session_config).expect("stub session spawns");
        let ext_handle = ExtensionHandle::start(Box::new(extension), session, 100);

        let state = RpcServerState::new();
        let adapter_id = "e1".to_string();
        let session_id = "s1".to_string();
        {
            let mut shared = state.inner.lock().expect("shared poisoned");
            shared.extensions.insert(
                adapter_id.clone(),
                Arc::new(Mutex::new(ExtensionEntry {
                    plugin: "stub-no-input".to_string(),
                    session: session_id.clone(),
                    handle: ext_handle,
                })),
            );
            shared
                .adapter_plugin
                .insert(adapter_id.clone(), "stub-no-input".to_string());
            shared
                .adapter_session
                .insert(adapter_id.clone(), session_id);
        }

        let mut server = RpcServer::with_state(state);
        let response = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":7,"method":"adapter.send","params":{"adapter":"e1","intent":"send_prompt","params":{"prompt":"hi"}}}"#,
        );
        assert_eq!(response["id"], 7);
        assert_eq!(
            response["error"]["code"], -32004,
            "expected -32004 PermissionDenied, got: {response}"
        );
        let message = response["error"]["message"]
            .as_str()
            .expect("error message present");
        assert!(
            message.contains("adapter.send"),
            "message should name the method: {message}"
        );
        assert!(
            message.contains("input.write"),
            "message should name the missing permission: {message}"
        );
        // Structured `data` lets programmatic callers react without regex
        // parsing the human-readable message.
        assert_eq!(
            response["error"]["data"]["method"], "adapter.send",
            "data.method should echo the rejected method: {response}"
        );
        assert_eq!(
            response["error"]["data"]["required_permission"], "input.write",
            "data.required_permission should name the missing permission: {response}"
        );
    }

    /// Control case: when the manifest declares every permission, the
    /// dispatcher must not block `adapter.send`. Drive a real bash
    /// claude-code-shaped manifest end-to-end through `adapter.start` +
    /// `adapter.close` to prove the allow path stays intact.
    ///
    /// `cfg(unix)` for the same reason as
    /// `adapter_send_denied_when_manifest_lacks_input_write`: the test
    /// spawns `/bin/sh` for the underlying PTY and Windows lacks it.
    #[test]
    #[cfg(unix)]
    fn adapter_send_allowed_with_built_in_claude_code_manifest() {
        let mut server = RpcServer::new();
        let start = handle(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","cat"]}}"#,
        );
        let adapter_id = start["result"]["adapter"]
            .as_str()
            .expect("adapter id present")
            .to_string();
        let send = handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"adapter.send","params":{{"adapter":"{adapter_id}","intent":"send_prompt","params":{{"prompt":"hello"}}}}}}"#
            ),
        );
        assert!(
            send["error"].is_null(),
            "send should succeed when manifest declares every permission: {send}"
        );
        // Cleanup so the cat process exits.
        handle(
            &mut server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"adapter.close","params":{{"adapter":"{adapter_id}"}}}}"#
            ),
        );
    }
}
