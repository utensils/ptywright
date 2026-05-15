//! Interactive REPL client for `ptywright serve`.
//!
//! Gated behind the `repl` Cargo feature so default builds stay slim and the
//! optional `reedline` / `ratatui` / `syntect` dependency stack is only paid
//! for when the operator opts in.
//!
//! The REPL is a JSON-RPC *client*: it connects to a running `ptywright
//! serve --socket <path>` (or spawns a child `ptywright serve --stdio`) and
//! drives the generic `adapter.*` surface. Application-specific behavior
//! (plugin names, intent strings) flows through as data — there are no
//! per-plugin Rust types here.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::error::Result;

pub mod command;
pub mod completer;
pub mod ctx;
pub mod highlighter;
pub mod history;
pub mod render;
pub mod snapshot;
pub mod socket;
pub mod spawn;
pub mod transport;

/// Wire framing for the JSON-RPC transport. Mirrors the server-side
/// `RpcFraming` enum in `src/main.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Newline-delimited JSON, one message per line.
    Ndjson,
    /// LSP-style Content-Length headers followed by JSON payloads.
    Lsp,
}

/// Transport selection for `ptywright repl`.
#[derive(Debug)]
pub enum Transport {
    /// Connect to a long-running `ptywright serve --socket <path>`.
    Socket(PathBuf),
    /// Spawn a child server and speak JSON-RPC over its stdio. The child
    /// command (e.g. `["ptywright", "serve", "--stdio"]`) is passed verbatim
    /// to `std::process::Command`.
    Stdio(Vec<String>),
}

/// Arguments for `ptywright repl`, threaded through from the CLI layer.
#[derive(Debug)]
pub struct ReplArgs {
    pub transport: Transport,
    pub framing: Framing,
}

/// Entry point invoked by `Commands::Repl` in `src/main.rs`. Builds the
/// transport, wires shared state, and hands off to the TUI event loop.
pub fn run(_args: ReplArgs) -> Result<ExitCode> {
    // Implementation is built up incrementally in follow-up commits on this
    // branch. Returning early here keeps the feature wire-able from the CLI
    // without forcing all submodules to land in a single change.
    Ok(ExitCode::SUCCESS)
}
