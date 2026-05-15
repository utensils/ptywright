//! Spawn a child `ptywright serve --stdio` (or any compatible server) and
//! pipe JSON-RPC over its stdio pair.
//!
//! Useful for ad-hoc REPL sessions where the operator has not started a
//! long-running daemon: `ptywright repl --stdio -- ptywright serve --stdio`.
//!
//! The child's stderr is captured by a small "tee" thread that forwards
//! every line into `tracing` at debug level so the REPL surface stays clean
//! while still preserving server-side diagnostics for `~/.ptywright/logs/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

use crate::error::{Error, Result};

/// Reader / writer halves wired to the child's stdio, plus a guard that
/// kills the child when dropped.
pub struct ChildTransport {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    _guard: ChildGuard,
}

/// Owns the spawned child process and the stderr-tee thread. Dropping the
/// guard kills the child (best-effort) and joins the tee.
pub struct ChildGuard {
    child: Option<Child>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn `command[0]` with `command[1..]` as args, capturing stdio. The
/// caller is responsible for picking a server-compatible argv — typically
/// `["ptywright", "serve", "--stdio"]` or `["/path/to/binary", "serve",
/// "--stdio", "--framing", "ndjson"]`.
pub fn spawn(command: &[String]) -> Result<ChildTransport> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::Rpc("--stdio requires a child command after `--`".to_string()))?;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Rpc(format!("spawn `{program}`: {error}")))?;

    let stdin: ChildStdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Rpc("child stdin was not piped".to_string()))?;
    let stdout: ChildStdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Rpc("child stdout was not piped".to_string()))?;
    let stderr: ChildStderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Rpc("child stderr was not piped".to_string()))?;

    let stderr_thread = std::thread::Builder::new()
        .name("ptywright-repl-child-stderr".into())
        .spawn(move || tee_stderr(stderr))
        .map_err(|error| Error::Rpc(format!("spawn stderr tee thread: {error}")))?;

    Ok(ChildTransport {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        _guard: ChildGuard {
            child: Some(child),
            stderr_thread: Some(stderr_thread),
        },
    })
}

fn tee_stderr(stderr: ChildStderr) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        match line {
            Ok(line) => tracing::debug!(target: "ptywright::repl::child", "{line}"),
            Err(error) => {
                tracing::debug!(target: "ptywright::repl::child", error = %error, "stderr read failed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::Framing;
    use crate::repl::transport::RpcClient;
    use serde_json::json;
    use std::time::Duration;

    /// Smoke test: spawn `cat` and verify that bytes round-trip through
    /// `ChildTransport`. We can't easily spawn a real `ptywright serve`
    /// from a unit test without the built binary on PATH, but `cat` is
    /// enough to prove the stdio plumbing works end-to-end.
    #[test]
    #[cfg(unix)]
    fn spawn_pipes_stdin_to_stdout_through_cat() {
        let transport = spawn(&["cat".to_string()]).expect("spawn cat");
        let ChildTransport {
            reader,
            mut writer,
            _guard,
        } = transport;

        writer.write_all(b"hello-stdio\n").expect("write");
        writer.flush().expect("flush");

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        assert_eq!(line, "hello-stdio\n");
    }

    /// Stronger smoke: spawn the test binary itself with a serve-like
    /// fixture that echoes back a single NDJSON response, then drive a
    /// real `RpcClient` against it. Mirrors how `Commands::Repl` will
    /// hand off to `RpcClient::new` in production.
    #[test]
    #[cfg(unix)]
    fn rpc_client_round_trips_over_spawned_child() {
        // We use `sh -c` so the fixture is a one-liner that doesn't
        // require maintaining a separate test-only binary. The shell
        // reads one line of input and emits a valid JSON-RPC response
        // with id=1 + result={"ok":true}.
        let script = r#"read line; printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'"#;
        let cmd = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        let transport = spawn(&cmd).expect("spawn sh fixture");
        let ChildTransport {
            reader,
            writer,
            _guard,
        } = transport;
        let client = RpcClient::new(reader, writer, Framing::Ndjson);
        let result = client
            .call("noop", json!({}), Duration::from_secs(2))
            .expect("rpc response from spawned child");
        assert_eq!(result["ok"], true);
    }
}
