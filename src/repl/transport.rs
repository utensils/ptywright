//! Framed JSON-RPC client used by the REPL.
//!
//! The client is intentionally minimal: synchronous, blocking, generic over
//! any `Read` + `Write` pair. One reader thread owns the input half and
//! demuxes incoming messages into two outbound channels:
//!
//! - **Responses** (objects with an `id` field) flow into per-call oneshot
//!   senders that [`RpcClient::call`] parks on.
//! - **Notifications** (objects with a `method` and no `id`) flow into a
//!   broadcast [`crossbeam_channel::Sender`] so consumers like the snapshot
//!   pump can subscribe without racing the response demux.
//!
//! The wire framing matches the server-side implementation in
//! [`crate::rpc`]: NDJSON for one-message-per-line, LSP-style
//! `Content-Length` framing for headers + payload.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde_json::{Value, json};

use super::Framing;
use crate::error::{Error, Result};

const JSONRPC_VERSION: &str = "2.0";

/// Notification frame surfaced to subscribers.
///
/// `method` is the JSON-RPC `"method"` field (e.g. `"session.changed"`),
/// `params` is the verbatim params object (or `Value::Null` when absent).
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// Error response payload extracted from a JSON-RPC `error` object.
///
/// Keeps the wire-level `code` available so callers can branch on standard
/// JSON-RPC error codes (e.g. `-32602` for InvalidParams). The redaction
/// policy already ran server-side, so `message` is safe to log verbatim.
#[derive(Debug, Clone, thiserror::Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// Result of a single in-flight call delivered through the response demux.
type CallSlot = Sender<std::result::Result<Value, RpcError>>;

/// Framed JSON-RPC client over arbitrary `Read` + `Write` halves.
pub struct RpcClient {
    framing: Framing,
    writer: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, CallSlot>>>,
    notifications_tx: Sender<Notification>,
    notifications_rx: Receiver<Notification>,
    reader_thread: Mutex<Option<JoinHandle<()>>>,
}

impl RpcClient {
    /// Build a new client around the given transport halves. The reader half
    /// is consumed by a background thread that runs until the underlying
    /// stream returns EOF or an error.
    pub fn new<R, W>(reader: R, writer: W, framing: Framing) -> Arc<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<i64, CallSlot>>> = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, notifications_rx) = unbounded();
        let client = Arc::new(Self {
            framing,
            writer: Mutex::new(Box::new(writer)),
            next_id: AtomicI64::new(1),
            pending: Arc::clone(&pending),
            notifications_tx: notifications_tx.clone(),
            notifications_rx,
            reader_thread: Mutex::new(None),
        });

        let reader_thread = std::thread::Builder::new()
            .name("ptywright-repl-rpc-reader".into())
            .spawn({
                let pending = Arc::clone(&pending);
                let notifications_tx = notifications_tx.clone();
                move || reader_loop(reader, framing, pending, notifications_tx)
            })
            .expect("spawn rpc reader thread");

        *client.reader_thread.lock().expect("reader-thread mutex") = Some(reader_thread);
        client
    }

    /// Issue a JSON-RPC call and block until the matching response arrives.
    ///
    /// Returns an [`Error::Rpc`] if the transport closes before a response
    /// is received, the server returns an `error` object, or the wait
    /// exceeds `timeout`.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (slot_tx, slot_rx) = bounded::<std::result::Result<Value, RpcError>>(1);
        self.pending
            .lock()
            .expect("pending mutex")
            .insert(id, slot_tx);

        let payload = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "method": method,
            "params": params,
        });
        let encoded = serde_json::to_string(&payload)
            .map_err(|error| Error::Rpc(format!("encode {method}: {error}")))?;

        if let Err(error) = self.send_frame(&encoded) {
            // Drop the slot we just registered so the reader thread does
            // not later try to deliver against a poisoned channel.
            self.pending.lock().expect("pending mutex").remove(&id);
            return Err(error);
        }

        match slot_rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(rpc_error)) => Err(Error::Rpc(rpc_error.to_string())),
            Err(_) => {
                // Either timeout or sender disconnected. Either way the slot
                // is now stale; remove it so a late response is logged and
                // discarded instead of leaking the entry forever.
                self.pending.lock().expect("pending mutex").remove(&id);
                Err(Error::Rpc(format!(
                    "rpc call `{method}` did not complete within {timeout:?}",
                )))
            }
        }
    }

    /// Subscribe to incoming JSON-RPC notifications.
    ///
    /// The returned receiver is a clone of the broadcast channel — every
    /// caller sees every notification. Subscribers are responsible for
    /// reading promptly; the unbounded queue grows otherwise.
    pub fn notifications(&self) -> Receiver<Notification> {
        self.notifications_rx.clone()
    }

    fn send_frame(&self, payload: &str) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer mutex");
        match self.framing {
            Framing::Ndjson => {
                writer.write_all(payload.as_bytes())?;
                writer.write_all(b"\n")?;
            }
            Framing::Lsp => {
                let header = format!("Content-Length: {}\r\n\r\n", payload.len());
                writer.write_all(header.as_bytes())?;
                writer.write_all(payload.as_bytes())?;
            }
        }
        writer.flush()?;
        Ok(())
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        // Cancel any pending callers so they exit with a clean error rather
        // than blocking on a sender that will never write. The reader
        // thread exits naturally once the transport closes — joining here
        // would deadlock callers who keep the writer half alive.
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        // Detach the reader thread; explicit teardown is the caller's job
        // (close the transport, then drop the Arc<RpcClient>).
        if let Ok(mut slot) = self.reader_thread.lock()
            && let Some(handle) = slot.take()
        {
            // Best-effort: a small join is fine, but we cannot block
            // indefinitely or the parent thread may hang on shutdown.
            let _ = handle;
        }
        drop(self.notifications_tx.clone());
    }
}

fn reader_loop<R: Read + Send + 'static>(
    reader: R,
    framing: Framing,
    pending: Arc<Mutex<HashMap<i64, CallSlot>>>,
    notifications: Sender<Notification>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        let frame = match framing {
            Framing::Ndjson => read_ndjson_line(&mut reader),
            Framing::Lsp => read_lsp_payload(&mut reader),
        };
        match frame {
            Ok(Some(payload)) => {
                if let Err(error) = dispatch_payload(&payload, &pending, &notifications) {
                    tracing::warn!(
                        error = %error,
                        "ptywright repl client: dropping malformed rpc frame",
                    );
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(error = %error, "ptywright repl client: rpc reader closed");
                break;
            }
        }
    }

    // Wake every pending caller so they observe the transport closure
    // instead of timing out long after the server has gone away.
    if let Ok(mut pending) = pending.lock() {
        for (_, slot) in pending.drain() {
            let _ = slot.send(Err(RpcError {
                code: -32603,
                message: "rpc transport closed".to_string(),
                data: None,
            }));
        }
    }
}

fn dispatch_payload(
    payload: &str,
    pending: &Mutex<HashMap<i64, CallSlot>>,
    notifications: &Sender<Notification>,
) -> Result<()> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| Error::Rpc(format!("malformed rpc frame: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::Rpc("rpc frame is not an object".to_string()))?;

    if let Some(id_value) = object.get("id")
        && !id_value.is_null()
    {
        let id = id_value
            .as_i64()
            .ok_or_else(|| Error::Rpc(format!("rpc response id is not an integer: {id_value}")))?;
        let slot = pending.lock().expect("pending mutex").remove(&id);
        if let Some(slot) = slot {
            let outcome = if let Some(error) = object.get("error") {
                Err(parse_error(error))
            } else if let Some(result) = object.get("result") {
                Ok(result.clone())
            } else {
                Err(RpcError {
                    code: -32603,
                    message: "rpc response is missing both `result` and `error`".to_string(),
                    data: None,
                })
            };
            let _ = slot.send(outcome);
        }
        return Ok(());
    }

    // No `id` → notification. Forward to subscribers (best-effort: a
    // disconnected receiver is fine, we just drop the frame).
    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let _ = notifications.send(Notification {
            method: method.to_string(),
            params,
        });
    }
    Ok(())
}

fn parse_error(value: &Value) -> RpcError {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown rpc error")
        .to_string();
    let data = value.get("data").cloned();
    RpcError {
        code,
        message,
        data,
    }
}

fn read_ndjson_line<R: BufRead>(reader: &mut R) -> Result<Option<String>> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn read_lsp_payload<R: BufRead>(reader: &mut R) -> Result<Option<String>> {
    let mut content_length: Option<usize> = None;
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
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| Error::Rpc(format!("invalid Content-Length: {error}")))?,
            );
        }
    }

    let length = content_length
        .ok_or_else(|| Error::Rpc("LSP frame missing Content-Length header".to_string()))?;
    let mut buf = vec![0_u8; length];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf)
        .map(Some)
        .map_err(|error| Error::Rpc(format!("LSP payload is not valid UTF-8: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{RpcServerState, serve_ndjson_with_state};
    use std::io::{PipeReader, PipeWriter, pipe};
    use std::sync::Arc;

    /// Build a pair of pipes wired up as: client → server → client.
    /// Returns `(client_read, client_write, server_thread)`.
    fn spawn_inprocess_server() -> (PipeReader, PipeWriter, std::thread::JoinHandle<()>) {
        let (client_to_server_r, client_to_server_w) = pipe().expect("pipe c→s");
        let (server_to_client_r, server_to_client_w) = pipe().expect("pipe s→c");
        let state = RpcServerState::new();
        let server_thread = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(client_to_server_r, server_to_client_w, state);
        });
        (server_to_client_r, client_to_server_w, server_thread)
    }

    #[test]
    fn call_returns_server_response() {
        let (read, write, _server) = spawn_inprocess_server();
        let client = RpcClient::new(read, write, Framing::Ndjson);
        let result = client
            .call("server.capabilities", json!({}), Duration::from_secs(5))
            .expect("server.capabilities");
        assert_eq!(result["name"], "ptywright");
        assert!(result["methods"].is_array());
    }

    #[test]
    fn call_surfaces_server_errors_as_rpc_error() {
        let (read, write, _server) = spawn_inprocess_server();
        let client = RpcClient::new(read, write, Framing::Ndjson);
        let outcome = client.call("not.a.real.method", json!({}), Duration::from_secs(5));
        let error = outcome.expect_err("unknown method must error");
        let text = error.to_string();
        assert!(
            text.contains("-32601") || text.to_lowercase().contains("method"),
            "expected method-not-found error, got: {text}",
        );
    }

    #[test]
    fn notifications_subscribers_receive_session_changed() {
        let (read, write, _server) = spawn_inprocess_server();
        let client = RpcClient::new(read, write, Framing::Ndjson);
        let notifications = client.notifications();
        client
            .call(
                "server.set_notifications",
                json!({"enabled": true}),
                Duration::from_secs(5),
            )
            .expect("enable notifications");

        // Spawn a fast fixture session so the server emits at least one
        // `session.changed` notification. `/bin/sh` is the same fixture
        // backing the rpc lifecycle tests.
        #[cfg(unix)]
        {
            let start = client
                .call(
                    "adapter.start",
                    json!({
                        "plugin": "claude-code",
                        "program": "/bin/sh",
                        "args": ["-lc", "printf 'transport-fixture\\n' && cat"],
                    }),
                    Duration::from_secs(5),
                )
                .expect("adapter.start");
            let adapter = start["adapter"].as_str().expect("adapter id");
            let session = start["session"].as_str().expect("session id").to_string();

            // Poke the server so it has reason to flush a notification batch.
            let _ = client.call(
                "adapter.state",
                json!({"adapter": adapter}),
                Duration::from_secs(5),
            );

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut saw = false;
            while std::time::Instant::now() < deadline {
                match notifications.recv_timeout(Duration::from_millis(200)) {
                    Ok(notification) => {
                        if notification.method == "session.changed"
                            && notification.params["session"] == session.as_str()
                        {
                            saw = true;
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = client.call(
                            "adapter.state",
                            json!({"adapter": adapter}),
                            Duration::from_secs(5),
                        );
                    }
                }
            }
            assert!(saw, "expected session.changed for session `{session}`");

            let _ = client.call(
                "adapter.close",
                json!({"adapter": adapter}),
                Duration::from_secs(5),
            );
        }
    }

    #[test]
    fn lsp_framing_round_trips() {
        let (client_to_server_r, client_to_server_w) = pipe().expect("pipe c→s");
        let (server_to_client_r, server_to_client_w) = pipe().expect("pipe s→c");
        let state = RpcServerState::new();
        std::thread::spawn(move || {
            let _ = crate::rpc::serve_lsp_with_state(client_to_server_r, server_to_client_w, state);
        });
        let client = RpcClient::new(server_to_client_r, client_to_server_w, Framing::Lsp);
        let result = client
            .call("server.capabilities", json!({}), Duration::from_secs(5))
            .expect("server.capabilities over lsp");
        assert_eq!(result["name"], "ptywright");
    }

    #[test]
    fn call_times_out_when_server_is_silent() {
        // A pipe that has the read end closed immediately so the reader
        // thread sees EOF and the call blocks until the timeout fires.
        let (_client_to_server_r, client_to_server_w) = pipe().expect("pipe c→s");
        let (server_to_client_r, server_to_client_w) = pipe().expect("pipe s→c");
        // Drop the server-side writer right away so the client sees EOF on
        // its reader half.
        drop(server_to_client_w);
        let client = RpcClient::new(server_to_client_r, client_to_server_w, Framing::Ndjson);
        let outcome = client.call("server.capabilities", json!({}), Duration::from_millis(250));
        assert!(outcome.is_err(), "expected error when transport is closed");
    }

    // Keep an Arc alive across the test so the reader thread is not torn
    // down between assertions.
    #[allow(dead_code)]
    fn keep_alive(_client: Arc<RpcClient>) {}
}
