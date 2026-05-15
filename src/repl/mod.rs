//! Interactive REPL client for `ptywright serve`.
//!
//! Gated behind the `repl` Cargo feature so default builds stay slim and the
//! optional `reedline` / `crossbeam-channel` / `nu-ansi-term` dependency
//! stack is only paid for when the operator opts in.
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
pub mod socket;
pub mod spawn;
pub mod transport;
pub mod tui;

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
pub fn run(args: ReplArgs) -> Result<ExitCode> {
    // `_child_guard` is held for the lifetime of the function so a stdio
    // child is killed and reaped on REPL exit. Socket transports get
    // `None` because no child was spawned.
    let _child_guard;
    let (reader, writer, label) = match args.transport {
        Transport::Socket(path) => {
            let transport = socket::connect(&path)?;
            _child_guard = None;
            (
                transport.reader,
                transport.writer,
                format!("socket:{}", path.display()),
            )
        }
        Transport::Stdio(command) => {
            let label = format!(
                "stdio:{}",
                command.first().map(String::as_str).unwrap_or("")
            );
            let (reader, writer, guard) = spawn::spawn(&command)?.into_parts();
            _child_guard = Some(guard);
            (reader, writer, label)
        }
    };
    let client = transport::RpcClient::new(reader, writer, args.framing);
    tui::run(client, label)?;
    Ok(ExitCode::SUCCESS)
}
