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

pub mod auto_serve;
pub mod completer;
pub mod ctx;
pub mod highlighter;
pub mod history;
pub mod lua;
pub mod meta;
pub mod socket;
pub mod spawn;
pub mod tips;
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
    /// Same shape as [`Socket`], but if no server is listening at `path`
    /// when the REPL starts up the REPL auto-spawns one in the
    /// background and prompts the operator on exit whether to shut it
    /// down or detach it. This is the default when `ptywright repl` is
    /// invoked with no transport flag.
    DefaultSocket(PathBuf),
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
    // `auto_server` is `Some` only when the REPL itself spawned the
    // server (the `DefaultSocket` no-server case). On REPL exit we
    // prompt the operator whether to shut it down or detach.
    let mut auto_server: Option<auto_serve::ManagedServer> = None;
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
        Transport::DefaultSocket(path) => {
            // Auto-spawn the server iff nothing's listening on the
            // socket yet. `socket_available` rejects both "file absent"
            // and "file exists but stale (no listener)" so a previously
            // crashed server can't masquerade as a healthy one. Pass
            // `args.framing` through so a `--framing lsp` REPL doesn't
            // end up shouting LSP frames at a server defaulting to
            // NDJSON.
            if !socket_available(&path) {
                auto_server = Some(auto_serve::spawn(&path, args.framing)?);
            }
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
    // Hand the operator a clean shutdown / detach choice for the
    // auto-spawned server. Errors from the prompt are non-fatal — we
    // never let prompt failure leak through as a REPL exit error.
    if let Some(server) = auto_server.take() {
        let _ = auto_serve::prompt_on_exit(server);
    }
    Ok(ExitCode::SUCCESS)
}

/// True iff the path resolves to a listening server. False when the path
/// is missing, when the file exists but no listener has bound to it
/// (stale socket from a crashed server), or when the connect handshake
/// fails for any reason.
fn socket_available(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericFilePath, Stream as LocalStream, prelude::*};
        match path.as_os_str().to_fs_name::<GenericFilePath>() {
            Ok(name) => LocalStream::connect(name).is_ok(),
            Err(_) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}
