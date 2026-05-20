//! Auto-spawn a `ptywright serve --socket <path>` child for the REPL.
//!
//! When the operator runs `ptywright repl` without any transport flag we
//! default to the per-user socket (`~/.ptywright/socket` or whatever
//! `PTYWRIGHT_HOME` points at). If no server is listening there yet, this
//! module starts one as a detached background child and waits for the
//! socket to become connectable. On REPL exit the operator gets prompted
//! whether to shut the auto-started server down or detach it so other
//! clients can keep using it.
//!
//! The behaviour is intentionally scoped to the "fresh-install" case —
//! the operator typed `ptywright repl` and got an "is the server running?"
//! error before this lived. If `--socket <path>` is passed explicitly we
//! never auto-spawn; the explicit path means the operator is connecting
//! to a server they manage themselves.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::Framing;
use crate::error::{Error, Result};

/// How long we'll wait for the auto-started server to publish its socket
/// before giving up. The server's normal startup is <50 ms, but we
/// generously allow up to five seconds for slow disks or first-build
/// scenarios.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Render a [`Framing`] into the CLI value enum string accepted by
/// `serve --framing <value>` (matches `RpcFraming` in `src/main.rs`).
/// Keeping the mapping here means an auto-spawn child speaks the same
/// wire format the REPL was configured for, instead of always
/// defaulting to NDJSON.
fn framing_cli_arg(framing: Framing) -> &'static str {
    match framing {
        Framing::Ndjson => "ndjson",
        Framing::Lsp => "lsp",
    }
}

/// Returned by [`spawn`] when auto-starting the server succeeded. Holds
/// the child handle + the socket path so [`prompt_on_exit`] can describe
/// what's still running.
///
/// The child handle is an `Option` so [`prompt_on_exit`] can `take` it
/// on the detach path — once detached the `Drop` safety net becomes a
/// no-op. The `Drop` impl kills any still-owned child as a panic /
/// early-return safety net.
pub struct ManagedServer {
    child: Option<Child>,
    path: PathBuf,
    pid: u32,
}

impl ManagedServer {
    /// PID of the auto-spawned server, for diagnostic messages.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// Spawn `ptywright serve --socket <path>` as a detached background
/// process and block until the socket is listening (up to
/// [`SERVER_READY_TIMEOUT`]). Returns a [`ManagedServer`] the REPL keeps
/// alongside the JSON-RPC client; on REPL exit [`prompt_on_exit`] either
/// shuts it down or detaches it.
///
/// The child is launched from the **current binary** (`std::env::current_exe()`)
/// so version skew is impossible — the auto-server is always the same
/// build that runs the REPL. Its stdio is fully detached
/// (`Stdio::null()`) so the child's stderr can't smash our terminal.
pub fn spawn(path: &Path, framing: Framing) -> Result<ManagedServer> {
    let exe = std::env::current_exe()
        .map_err(|error| Error::Rpc(format!("auto-server: locate current executable: {error}")))?;

    eprintln!(
        "ptywright: no server at {} — starting one in the background…",
        path.display(),
    );

    let mut command = Command::new(&exe);
    command
        .arg("serve")
        .arg("--socket")
        .arg(path)
        .arg("--framing")
        .arg(framing_cli_arg(framing))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // On Unix, place the child in its own process group so an
    // accidental Ctrl-C delivered to the REPL doesn't tear down the
    // auto-server (the operator may want to detach it). The REPL still
    // controls lifetime through SIGTERM / `kill()` in `prompt_on_exit`.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() before exec is safe — it runs in the forked
        // child and has no Rust-visible effect on the parent process.
        unsafe {
            command.pre_exec(|| {
                // A freshly-forked child is never already a process
                // group leader, so setsid() cannot realistically fail
                // here — but if it ever did, the "own process group"
                // guarantee would be silently broken. Surface the
                // failure as an `io::Error` so `spawn()` reports it
                // instead of starting a server that a stray Ctrl-C
                // could tear down.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command
        .spawn()
        .map_err(|error| Error::Rpc(format!("auto-server: spawn {exe:?}: {error}")))?;
    let pid = child.id();
    let server = ManagedServer {
        child: Some(child),
        path: path.to_path_buf(),
        pid,
    };

    wait_for_socket(path, SERVER_READY_TIMEOUT)?;
    eprintln!(
        "ptywright: server ready · pid {} · socket {}",
        server.pid(),
        path.display(),
    );
    Ok(server)
}

/// Poll the socket path until it accepts a connection or `timeout`
/// elapses. We can't just wait for `path.exists()` — the server creates
/// the file slightly before it starts accepting on it.
///
/// `timeout` is a parameter rather than a hard-coded constant so the
/// timeout-path test can exercise the deadline branch with a short
/// budget instead of blocking the whole suite for [`SERVER_READY_TIMEOUT`].
fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if can_connect(path) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(Error::Rpc(format!(
        "auto-server: socket {} did not start accepting connections within {:?}",
        path.display(),
        timeout,
    )))
}

#[cfg(unix)]
fn can_connect(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(windows)]
fn can_connect(path: &Path) -> bool {
    use interprocess::local_socket::{GenericFilePath, Stream as LocalStream, prelude::*};
    match path.as_os_str().to_fs_name::<GenericFilePath>() {
        Ok(name) => LocalStream::connect(name).is_ok(),
        Err(_) => false,
    }
}

#[cfg(not(any(unix, windows)))]
fn can_connect(_path: &Path) -> bool {
    false
}

/// Ask the operator whether to shut down the auto-started server or
/// leave it running for sibling connections to use. Default (empty input
/// or non-interactive stdin) is shutdown — the auto-server is implicitly
/// scoped to this REPL session, so detach is the explicit ask.
pub fn prompt_on_exit(mut server: ManagedServer) -> Result<()> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    eprintln!();
    eprintln!(
        "ptywright: auto-started server still running · pid {} · socket {}",
        server.pid(),
        server.path.display(),
    );
    let detach = if interactive {
        eprint!("Shut it down? [Y/n / detach] ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => false, // EOF / read failure → shutdown
            Ok(_) => {
                let trimmed = line.trim().to_ascii_lowercase();
                matches!(trimmed.as_str(), "n" | "no" | "d" | "detach" | "leave")
            }
        }
    } else {
        // Non-interactive stdin (piped, redirected): default to shutdown
        // so a scripted REPL invocation doesn't leak a daemon process.
        false
    };

    if detach {
        eprintln!(
            "ptywright: detaching · server still listening on {} · pid {}",
            server.path.display(),
            server.pid(),
        );
        // Take the child out so `Drop` sees `None` and leaves it alone.
        // The OS owns the process from here; the operator can later
        // shut it down with `kill <pid>` or by connecting and sending
        // `:rpc server.shutdown` (if/when that lands).
        let _ = server.child.take();
    } else {
        eprintln!("ptywright: shutting down auto-started server…");
        // Best-effort: SIGKILL via `Child::kill`. The server installs
        // no signal handlers so we skip the SIGTERM ladder for now;
        // bumping this to SIGTERM-then-SIGKILL is a future polish.
        if let Some(mut child) = server.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    Ok(())
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        // Safety net for panics or early returns: if the operator never
        // reaches `prompt_on_exit`, the server still gets shut down so
        // an abandoned daemon doesn't accumulate across crashes. The
        // detach path explicitly `take()`s the child first, so this is
        // a no-op in that case.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_socket_times_out_when_no_server_appears() {
        // Use a definitely-nonexistent path with a short injected
        // timeout — the deadline branch is what's under test, and a
        // 200 ms budget exercises it without blocking the suite for
        // the full production `SERVER_READY_TIMEOUT`.
        let path =
            std::env::temp_dir().join(format!("ptywright-auto-test-{}.sock", std::process::id()));
        let budget = Duration::from_millis(200);
        let start = Instant::now();
        let outcome = wait_for_socket(&path, budget);
        let elapsed = start.elapsed();
        assert!(outcome.is_err(), "expected timeout error for absent socket");
        assert!(
            elapsed >= budget,
            "wait must run the full timeout window before erroring; took {elapsed:?}"
        );
        assert!(
            elapsed < budget + Duration::from_secs(2),
            "wait should not exceed the timeout by more than a small slack; took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn can_connect_returns_false_for_nonexistent_socket() {
        let path =
            std::env::temp_dir().join(format!("ptywright-auto-conn-{}.sock", std::process::id()));
        assert!(!can_connect(&path));
    }

    #[test]
    fn framing_cli_arg_round_trips_both_wire_formats() {
        // Pin the strings — the server's `--framing` flag is wired via
        // clap's `ValueEnum` derive, which lowercases the enum variants
        // (`ndjson` / `lsp`). If `RpcFraming`'s naming ever changes,
        // this guard fails before the auto-spawn path goes silently
        // mismatched.
        assert_eq!(framing_cli_arg(Framing::Ndjson), "ndjson");
        assert_eq!(framing_cli_arg(Framing::Lsp), "lsp");
    }
}
