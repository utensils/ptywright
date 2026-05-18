use std::io;

use crate::plugin::PluginPermission;

/// ptywright result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for PTY/session operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// PTY backend error.
    #[error("pty backend error: {0}")]
    Pty(#[from] anyhow::Error),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// JSON parsing or encoding error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Timed out waiting for a matcher.
    #[error("timed out waiting for matcher")]
    Timeout,
    /// Reader thread ended unexpectedly.
    #[error("session output reader ended")]
    ReaderEnded,
    /// Child process has already exited or closed its PTY.
    #[error("session is closed")]
    Closed,
    /// Lua plugin runtime error.
    #[error("lua plugin error: {0}")]
    Lua(String),
    /// RPC protocol error.
    #[error("rpc error: {0}")]
    Rpc(String),
    /// Failed to parse the on-disk config file.
    #[error("config error: {0}")]
    Config(String),
    /// JSON-RPC method denied because the bound plugin manifest does not
    /// declare the required permission. Surfaced over the wire as JSON-RPC
    /// error code `-32004`, with structured `data` carrying the method name
    /// and the missing permission.
    #[error("method `{method}` requires permission `{}`", required.as_str())]
    PermissionDenied {
        /// JSON-RPC method that was rejected.
        method: String,
        /// Permission the caller's plugin manifest needed to declare.
        required: PluginPermission,
    },
    /// Operation has no equivalent on the current platform — see the
    /// per-platform notes on [`crate::Signal`] for the signal subset that
    /// maps cleanly to Windows.
    #[error("unsupported on this platform: {0}")]
    UnsupportedOnPlatform(String),
}
