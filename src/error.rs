use std::io;

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
    /// RPC protocol error.
    #[error("rpc error: {0}")]
    Rpc(String),
}
