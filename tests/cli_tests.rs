use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ptywright"))
}

#[cfg(unix)]
fn echo_tui_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_ptywright-echo-tui")
}

#[test]
fn prints_help_by_default() {
    let output = bin().output().expect("run ptywright");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(
        "A cross-platform Rust CLI and library for driving interactive terminal applications through PTYs"
    ));
    assert!(stdout.contains("Usage: ptywright"));
    assert!(stdout.contains("--version"));
}

#[test]
#[cfg(unix)]
fn run_executes_command_in_pty() {
    let mut command = bin();
    command.arg("run").arg("--");
    if cfg!(windows) {
        command.args(["cmd.exe", "/C", "echo cli-ready"]);
    } else {
        command.args(["/bin/sh", "-lc", "printf cli-ready"]);
    }

    let output = command.output().expect("run ptywright run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("cli-ready"));
}

#[test]
#[cfg(unix)]
fn run_bridges_stdin_to_child() {
    let mut child = bin()
        .args(["run", "--", "/bin/sh", "-lc", "read line; printf got:$line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright run");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("got:hello"));
}

#[test]
fn serve_stdio_returns_json_rpc_response() {
    let mut child = bin()
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server.capabilities\"}\n")
        .expect("write request");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright serve");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(
        response["result"]["methods"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("session.create"))
    );
}

#[test]
fn serve_stdio_lsp_returns_json_rpc_response() {
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.list\"}";
    let mut child = bin()
        .args(["serve", "--stdio", "--framing", "lsp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve");

    write!(
        child.stdin.as_mut().expect("stdin"),
        "Content-Length: {}\r\n\r\n",
        request.len()
    )
    .expect("write headers");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request)
        .expect("write request");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright serve");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let (_, payload) = stdout.split_once("\r\n\r\n").expect("lsp separator");
    let response: serde_json::Value = serde_json::from_str(payload).expect("json response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["sessions"], serde_json::json!([]));
}

#[test]
#[cfg(unix)]
fn serve_unix_socket_returns_json_rpc_response() {
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = std::env::temp_dir().join(format!(
        "ptywright-test-{}-{unique}.sock",
        std::process::id()
    ));

    let mut child = bin()
        .args(["serve", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --socket");

    let mut stream = None;
    for _ in 0..100 {
        match UnixStream::connect(&socket) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let mut stream = stream.expect("connect to socket");
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.list\"}\n")
        .expect("write request");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");
    let mut stdout = String::new();
    stream.read_to_string(&mut stdout).expect("read response");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&socket);

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["sessions"], serde_json::json!([]));
}

#[test]
#[cfg(unix)]
fn serve_unix_socket_unlinks_socket_on_sigterm() {
    // Regression: previously `ptywright serve --socket …` left the socket
    // file on disk on graceful exit, so the next `ptywright repl` saw a
    // stale socket and reported a confusing "is a stale server file
    // lingering?" error. The shutdown signal handler installed in
    // `serve_socket` should unlink the socket before the process exits.
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = std::env::temp_dir().join(format!(
        "ptywright-test-cleanup-{}-{unique}.sock",
        std::process::id()
    ));

    let mut child = bin()
        .args(["serve", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --socket");

    // Wait for the server to bind the socket.
    let mut bound = false;
    for _ in 0..200 {
        if UnixStream::connect(&socket).is_ok() {
            bound = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(bound, "server never bound socket {socket:?}");
    assert!(
        socket.exists(),
        "socket file must exist while server is running"
    );

    // Send SIGTERM. The signal handler should unlink the file before
    // `_exit`.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = child.wait();

    // Poll briefly to allow the OS to reflect the unlink; the handler
    // runs synchronously before `_exit` but the parent's filesystem
    // view can lag the syscall on some platforms.
    let mut removed = false;
    for _ in 0..50 {
        if !socket.exists() {
            removed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        removed,
        "socket file should have been unlinked on SIGTERM, still exists at {socket:?}"
    );
}

#[test]
#[cfg(unix)]
fn serve_unix_socket_shares_sessions_across_connections() {
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = std::env::temp_dir().join(format!(
        "ptywright-test-{}-{unique}.sock",
        std::process::id()
    ));

    let mut child = bin()
        .args(["serve", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --socket");

    let connect = || {
        for _ in 0..100 {
            match UnixStream::connect(&socket) {
                Ok(connected) => return connected,
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("connect to socket")
    };

    let mut first = connect();
    first
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.create\",\"params\":{\"program\":\"/bin/sh\",\"args\":[\"-lc\",\"sleep 1\"]}}\n")
        .expect("write create");
    first
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown first write");
    let mut create_stdout = String::new();
    first
        .read_to_string(&mut create_stdout)
        .expect("read create response");
    let create: serde_json::Value =
        serde_json::from_str(create_stdout.trim()).expect("json create response");
    let session = create["result"]["session"].as_str().expect("session id");

    let mut second = connect();
    second
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session.list\"}\n")
        .expect("write list");
    second
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown second write");
    let mut list_stdout = String::new();
    second
        .read_to_string(&mut list_stdout)
        .expect("read list response");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&socket);

    let list: serde_json::Value = serde_json::from_str(list_stdout.trim()).expect("json list");
    assert!(
        list["result"]["sessions"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(session))
    );
}

#[test]
#[cfg(unix)]
fn run_writes_log_file_under_ptywright_home() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!("ptywright-home-{}-{unique}", std::process::id()));

    let output = bin()
        .env("PTYWRIGHT_HOME", &home)
        .env("PTYWRIGHT_LOG", "info")
        .args(["run", "--", "/bin/sh", "-lc", "printf cli-ready"])
        .output()
        .expect("run ptywright run");

    assert!(output.status.success(), "ptywright run failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("cli-ready"));

    let logs_dir = home.join("logs");
    assert!(
        logs_dir.exists(),
        "logs dir not created at {}",
        logs_dir.display()
    );

    let entries: Vec<_> = std::fs::read_dir(&logs_dir)
        .expect("read logs dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|name| name.starts_with("ptywright")),
        "expected a ptywright.* log file in {}, got {entries:?}",
        logs_dir.display()
    );

    let _ = std::fs::remove_dir_all(home);
}

fn dynamic_completions(shell: &str, index: &str, words: &[&str]) -> String {
    let output = bin()
        .env("COMPLETE", shell)
        .env("_CLAP_COMPLETE_INDEX", index)
        .args(["--"])
        .args(words)
        .output()
        .expect("run dynamic completion");
    assert!(output.status.success(), "completion failed: {output:?}");
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

#[test]
fn dynamic_root_completions_include_run_and_serve() {
    let stdout = dynamic_completions("bash", "1", &["ptywright", ""]);

    assert!(
        stdout.lines().any(|line| line == "run"),
        "root completions should include run, got: {stdout}"
    );
    assert!(
        stdout.lines().any(|line| line == "serve"),
        "root completions should include serve, got: {stdout}"
    );
}

#[test]
fn dynamic_completions_include_run_flags() {
    let stdout = dynamic_completions("bash", "2", &["ptywright", "run", "--"]);

    assert!(
        stdout.lines().any(|line| line.starts_with("--rows")),
        "run completions should include --rows, got: {stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("--cols")),
        "run completions should include --cols, got: {stdout}"
    );
}

#[test]
fn completions_bash_outputs_script() {
    let output = bin()
        .args(["completions", "bash"])
        .output()
        .expect("run ptywright completions bash");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains("COMPLETE") || stdout.contains("complete"),
        "bash completions should contain registration, got: {stdout}"
    );
}

#[test]
fn completions_zsh_outputs_script() {
    let output = bin()
        .args(["completions", "zsh"])
        .output()
        .expect("run ptywright completions zsh");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.starts_with("#compdef ptywright"));
    assert!(stdout.contains("_clap_dynamic_completer_ptywright"));
}

#[test]
fn completions_fish_outputs_script() {
    let output = bin()
        .args(["completions", "fish"])
        .output()
        .expect("run ptywright completions fish");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("complete") && stdout.contains("ptywright"));
}

#[test]
fn completions_unknown_shell_errors() {
    let output = bin()
        .args(["completions", "tcsh"])
        .output()
        .expect("run ptywright completions tcsh");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(stderr.contains("unknown shell 'tcsh'"));
}

#[test]
fn prints_version() {
    let output = bin()
        .arg("--version")
        .output()
        .expect("run ptywright --version");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert_eq!(
        stdout.trim(),
        format!("ptywright {}", env!("CARGO_PKG_VERSION"))
    );
}

/// End-to-end integration test (Milestone 21.8): drive a tiny in-tree TUI
/// fixture binary (`ptywright-echo-tui`) through the JSON-RPC server using
/// only the generic `session.*` methods. This proves ptywright can drive an
/// arbitrary PTY-backed program end-to-end without depending on Claude Code
/// or any external tool.
///
/// The fixture prints `READY> `, reads a line and echoes it as `> <line>`,
/// then on a blank line prints `<answer>OK</answer>` and exits.
///
/// Round-trip exercised here:
///   1. `session.create` spawns `ptywright-echo-tui` in a PTY.
///   2. `session.wait` (contains_text "READY>" + screen_stable) — prompt up.
///   3. `session.input` sends "hello" + enter, then a bare enter (blank line).
///   4. `session.wait` (contains_text "<answer>OK</answer>" + screen_stable).
///   5. `session.snapshot` returns plain_text containing the answer marker.
///   6. `session.kill` cleans up.
#[test]
#[cfg(unix)]
fn end_to_end_session_round_trip_with_echo_tui_fixture() {
    use std::io::{BufRead, BufReader};

    // stderr is `Stdio::null()` (not `piped()`) to match the rest of this
    // file. Capturing stderr without draining it lets the server's pipe
    // buffer fill on chatty `RUST_LOG`/`PTYWRIGHT_LOG` configurations and
    // deadlocks the child — the very kind of CI hang we don't want in this
    // round-trip test.
    let mut child = bin()
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --stdio");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // Send one NDJSON request and read exactly one response line.
    let mut request_one = |line: String| -> serde_json::Value {
        stdin.write_all(line.as_bytes()).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
        let mut buf = String::new();
        let n = stdout.read_line(&mut buf).expect("read response line");
        assert!(n > 0, "server closed stdout before responding");
        serde_json::from_str(buf.trim()).expect("json response")
    };

    // 1. session.create with the in-tree echo_tui fixture binary.
    let create = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session.create",
        "params": {
            "program": echo_tui_bin_path(),
            "rows": 24,
            "cols": 80,
        },
    });
    let create_resp = request_one(create.to_string());
    assert_eq!(create_resp["id"], 1);
    let session = create_resp["result"]["session"]
        .as_str()
        .expect("session id returned")
        .to_owned();

    // 2. Wait for the prompt to render and the screen to settle.
    let wait_prompt = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session.wait",
        "params": {
            "session": session,
            "matcher": {"type": "all", "value": [
                {"type": "contains_text", "value": "READY>"},
                {"type": "screen_stable", "value": {"min_ms": 250}},
            ]},
            "timeout_ms": 20_000,
        },
    });
    let wait_prompt_resp = request_one(wait_prompt.to_string());
    assert_eq!(wait_prompt_resp["id"], 2);
    assert!(
        wait_prompt_resp["result"]["matched"]
            .as_bool()
            .unwrap_or(false),
        "expected matched=true waiting for prompt, got: {wait_prompt_resp}"
    );

    // 3a. Send "hello" then Enter (the fixture echoes it as `> hello`).
    let send_hello = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session.input",
        "params": {
            "session": session,
            "action": {"type": "text", "value": "hello"},
        },
    });
    let send_hello_resp = request_one(send_hello.to_string());
    assert_eq!(send_hello_resp["result"]["sent"], serde_json::json!(true));

    let press_enter = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session.input",
        "params": {
            "session": session,
            "action": {"type": "key", "value": "enter"},
        },
    });
    let press_enter_resp = request_one(press_enter.to_string());
    assert_eq!(press_enter_resp["result"]["sent"], serde_json::json!(true));

    // 3b. Send a blank line (just Enter) to trigger the answer + exit.
    let blank_line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session.input",
        "params": {
            "session": session,
            "action": {"type": "key", "value": "enter"},
        },
    });
    let blank_line_resp = request_one(blank_line.to_string());
    assert_eq!(blank_line_resp["result"]["sent"], serde_json::json!(true));

    // 4. Wait for the answer marker to appear and the screen to settle.
    let wait_answer = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "session.wait",
        "params": {
            "session": session,
            "matcher": {"type": "all", "value": [
                {"type": "contains_text", "value": "<answer>OK</answer>"},
                {"type": "screen_stable", "value": {"min_ms": 250}},
            ]},
            "timeout_ms": 20_000,
        },
    });
    let wait_answer_resp = request_one(wait_answer.to_string());
    assert_eq!(wait_answer_resp["id"], 6);
    assert!(
        wait_answer_resp["result"]["matched"]
            .as_bool()
            .unwrap_or(false),
        "expected matched=true waiting for answer, got: {wait_answer_resp}"
    );

    // 5. Snapshot and assert the plain_text contains the answer marker.
    let snapshot = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "session.snapshot",
        "params": {"session": session},
    });
    let snapshot_resp = request_one(snapshot.to_string());
    let plain_text = snapshot_resp["result"]["plain_text"]
        .as_str()
        .expect("plain_text string");
    assert!(
        plain_text.contains("<answer>OK</answer>"),
        "snapshot.plain_text missing answer marker; got: {plain_text:?}"
    );
    assert!(
        plain_text.contains("> hello"),
        "snapshot.plain_text missing echoed line; got: {plain_text:?}"
    );

    // 6. Clean up.
    let kill = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "session.kill",
        "params": {"session": session},
    });
    let _ = request_one(kill.to_string());

    drop(stdin);
    let _ = child.wait();
}

/// Verify LSP framing handles back-to-back requests on a single
/// connection. NDJSON splits on `\n` so message boundaries are free; LSP
/// framing needs `Content-Length: N\r\n\r\n<N bytes>` per message and has
/// historically been the surface that exposes framing-state bugs (header
/// re-use, payload truncation, multi-byte boundary skipping). Two
/// requests with different payload sizes is the smallest combination
/// that catches header-state regressions.
#[test]
fn serve_stdio_lsp_handles_two_back_to_back_requests() {
    use std::io::BufReader;

    fn write_lsp_message(out: &mut impl Write, payload: &[u8]) {
        write!(out, "Content-Length: {}\r\n\r\n", payload.len()).expect("write headers");
        out.write_all(payload).expect("write payload");
    }

    fn read_lsp_message(reader: &mut BufReader<impl Read>) -> serde_json::Value {
        // Read headers line-by-line until the blank `\r\n` separator. The
        // spec allows additional headers; we only care about Content-Length.
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            let n = std::io::BufRead::read_line(reader, &mut header).expect("read header line");
            assert!(n > 0, "stream closed inside header block");
            if header == "\r\n" {
                break;
            }
            if let Some(rest) = header
                .strip_prefix("Content-Length:")
                .or_else(|| header.strip_prefix("content-length:"))
            {
                content_length = rest.trim().parse().expect("parse content-length");
            }
        }
        assert!(content_length > 0, "server returned empty Content-Length");
        let mut buf = vec![0u8; content_length];
        std::io::Read::read_exact(reader, &mut buf).expect("read payload bytes");
        serde_json::from_slice(&buf).expect("payload is json")
    }

    let mut child = bin()
        .args(["serve", "--stdio", "--framing", "lsp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --framing lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // First request: short payload — `server.capabilities`.
    let first = br#"{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}"#;
    write_lsp_message(&mut stdin, first);
    let r1 = read_lsp_message(&mut stdout);
    assert_eq!(r1["id"], 1);
    assert!(r1["result"]["methods"].is_array());

    // Second request: longer payload — `adapter.list` plus an explicit
    // params object so the JSON is materially different in size from the
    // first message. A framing-state bug that re-reads the prior
    // Content-Length would either truncate this message or split it
    // across "boundaries" and fail to parse.
    let second = br#"{"jsonrpc":"2.0","id":2,"method":"adapter.list","params":{}}"#;
    write_lsp_message(&mut stdin, second);
    let r2 = read_lsp_message(&mut stdout);
    assert_eq!(r2["id"], 2);
    assert!(
        r2["result"]["plugins"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "expected at least one plugin in adapter.list response; got {r2}",
    );

    drop(stdin);
    let _ = child.wait();
}

/// Verify `server.set_notifications` actually enables server-originated
/// `session.changed` deliveries on the current connection and that the
/// response to a request always precedes any queued notification.
#[test]
#[cfg(unix)]
fn serve_stdio_emits_session_changed_after_opt_in() {
    use std::io::{BufRead, BufReader};

    let mut child = bin()
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --stdio");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let mut read_line = || -> serde_json::Value {
        let mut buf = String::new();
        let n = stdout.read_line(&mut buf).expect("read line");
        assert!(n > 0, "server closed stdout");
        serde_json::from_str(buf.trim()).expect("json line")
    };

    let write = |stdin: &mut std::process::ChildStdin, line: &str| {
        stdin.write_all(line.as_bytes()).expect("write");
        stdin.write_all(b"\n").expect("write newline");
    };

    // 1. Opt in to notifications. Response must come first.
    write(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"server.set_notifications","params":{"enabled":true}}"#,
    );
    let opt_in = read_line();
    assert_eq!(opt_in["id"], 1);
    assert_eq!(opt_in["result"]["enabled"], true);

    // 2. Create a session that immediately writes a byte. The PTY output
    //    bumps the session sequence and should drive a `session.changed`
    //    notification on the next message handler turn.
    let create = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session.create",
        "params": {
            "program": "/bin/sh",
            "args": ["-lc", "printf hello; sleep 2"],
            "rows": 24,
            "cols": 80,
        },
    });
    write(&mut stdin, &create.to_string());
    let create_resp = read_line();
    assert_eq!(create_resp["id"], 2, "create response: {create_resp}");
    let session = create_resp["result"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();

    // 3. Drive a `session.wait` for the printed bytes. Notifications are
    //    poll-coalesced at the end of each handler turn, so emitted
    //    notifications can interleave with subsequent request responses
    //    (a notification queued by the create handler may arrive before
    //    the wait response if both are read from stdout in sequence).
    //    The per-turn ordering guarantee we actually care about is:
    //    within a single handler invocation the response precedes any
    //    notifications queued by that same turn. That's enforced in the
    //    unit tests in src/rpc.rs (`notification_subscriptions_*`); here
    //    we just verify the wire integration delivers both messages and
    //    that the wait response is correct.
    let wait = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session.wait",
        "params": {
            "session": session,
            "matcher": {"type": "contains_text", "value": "hello"},
            "timeout_ms": 5000,
        },
    });
    write(&mut stdin, &wait.to_string());

    let mut saw_response = false;
    let mut saw_notification = false;
    for _ in 0..16 {
        let msg = read_line();
        if msg["id"] == 3 {
            assert!(
                msg["result"]["matched"].as_bool().unwrap_or(false),
                "wait did not match: {msg}",
            );
            saw_response = true;
        } else if msg["method"] == "session.changed" {
            assert!(msg["params"]["session"].is_string());
            assert!(msg["params"]["sequence"].is_number());
            saw_notification = true;
        }
        if saw_response && saw_notification {
            break;
        }
    }
    assert!(saw_response, "never observed wait response");
    assert!(
        saw_notification,
        "never observed session.changed notification"
    );

    // 4. Clean up.
    let kill = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "session.kill",
        "params": {"session": session},
    });
    write(&mut stdin, &kill.to_string());
    let _ = read_line();

    drop(stdin);
    let _ = child.wait();
}

// ---- Trusted-local third-party plugin loading (GH #16) -----------------

/// Path to the canonical `echo` fixture plugin used by the third-party
/// loading tests.
fn echo_plugin_manifest_path() -> std::path::PathBuf {
    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set during cargo test");
    std::path::PathBuf::from(manifest)
        .join("tests")
        .join("fixtures")
        .join("plugins")
        .join("echo")
        .join("manifest.toml")
}

#[test]
fn serve_loads_third_party_plugin_via_cli_flag() {
    let manifest_path = echo_plugin_manifest_path();
    let mut child = bin()
        .args([
            "serve",
            "--stdio",
            "--plugin",
            manifest_path.to_str().expect("manifest path is utf8"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --plugin");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"adapter.list\"}\n")
        .expect("write adapter.list");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright serve");
    assert!(output.status.success(), "serve exited non-zero: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    let names: Vec<String> = response["result"]["plugins"]
        .as_array()
        .expect("plugins array")
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "claude-code"),
        "built-in claude-code must remain visible after --plugin load: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "echo"),
        "echo plugin must be visible after --plugin load: {names:?}"
    );
}

#[test]
fn plugin_load_denied_without_allow_flag() {
    let manifest_path = echo_plugin_manifest_path();
    let request = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.load\",\"params\":{{\"manifest_path\":{:?}}}}}\n",
        manifest_path.to_str().expect("manifest path is utf8")
    );
    let mut child = bin()
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.as_bytes())
        .expect("write plugin.load");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert_eq!(
        response["error"]["code"], -32004,
        "plugin.load without --allow-plugin-load must return -32004: {response}"
    );
    assert_eq!(
        response["error"]["data"]["reason"], "server_did_not_grant_plugin_load",
        "data.reason must distinguish server-mode denial: {response}"
    );
}

// `cfg(unix)` because adapter.start spawns the echo manifest's
// `default_target.program = "/bin/sh"`; the Windows CI job runs
// `cargo test --locked` (see .github/workflows/ci.yml) and would fail to
// spawn /bin/sh. The other two tests in this section only exercise
// `adapter.list` / `plugin.load` which don't spawn a child, so they stay
// cross-platform.
#[test]
#[cfg(unix)]
fn plugin_load_rejects_duplicate_registration() {
    // Loading the same manifest twice exercises the
    // `RpcServerState::register_plugin` collision path (Error::Config
    // "plugin already registered"). The first load via --plugin seeds the
    // registry; the second via plugin.load (with --allow-plugin-load) must
    // bounce with -32603 because Error::Config maps to InternalError.
    let manifest_path = echo_plugin_manifest_path();
    let manifest_str = manifest_path.to_str().expect("manifest path is utf8");
    let request = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.load\",\"params\":{{\"manifest_path\":{manifest_str:?}}}}}\n"
    );
    let mut child = bin()
        .args([
            "serve",
            "--stdio",
            "--allow-plugin-load",
            "--plugin",
            manifest_str,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --plugin + --allow-plugin-load");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.as_bytes())
        .expect("write plugin.load");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert!(
        response["error"].is_object(),
        "second plugin.load must fail when the name is already registered: {response}"
    );
    let message = response["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("already registered"),
        "error should explain the duplicate registration: {message}"
    );
}

#[test]
#[cfg(unix)]
fn plugin_unload_rejects_unknown_plugin() {
    let mut child = bin()
        .args(["serve", "--stdio", "--allow-plugin-load"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --allow-plugin-load");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.unload\",\"params\":{\"plugin\":\"never-registered\"}}\n")
        .expect("write plugin.unload");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert_eq!(
        response["error"]["code"], -32602,
        "unknown plugin must return InvalidParams: {response}"
    );
    let message = response["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("unknown plugin"),
        "error should name the unknown plugin: {message}"
    );
}

#[test]
#[cfg(unix)]
fn plugin_unload_refuses_to_remove_built_in() {
    // `claude-code` is bundled into BUILTIN_PLUGINS and seeded into the
    // registry on startup with builtin: true. The unload handler must
    // refuse to remove it even when --allow-plugin-load is set.
    let mut child = bin()
        .args(["serve", "--stdio", "--allow-plugin-load"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --allow-plugin-load");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.unload\",\"params\":{\"plugin\":\"claude-code\"}}\n")
        .expect("write plugin.unload");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json response");
    assert_eq!(
        response["error"]["code"], -32602,
        "built-in unload attempt must return InvalidParams: {response}"
    );
    let message = response["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("built in"),
        "error should explain the built-in protection: {message}"
    );
}

#[test]
#[cfg(unix)]
fn plugin_unload_happy_path_removes_third_party() {
    // Load echo via plugin.load, then unload it, then assert adapter.list
    // no longer contains it.
    let manifest_path = echo_plugin_manifest_path();
    let manifest_str = manifest_path.to_str().expect("manifest path is utf8");
    let requests = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.load\",\"params\":{{\"manifest_path\":{path:?}}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"plugin.unload\",\"params\":{{\"plugin\":\"echo\"}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"adapter.list\"}}\n",
        ),
        path = manifest_str,
    );
    let mut child = bin()
        .args(["serve", "--stdio", "--allow-plugin-load"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --allow-plugin-load");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(requests.as_bytes())
        .expect("write requests");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert_eq!(responses[0]["result"]["plugin"], "echo");
    assert_eq!(responses[1]["result"]["unloaded"], true);
    let names: Vec<String> = responses[2]["result"]["plugins"]
        .as_array()
        .expect("plugins array")
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !names.iter().any(|n| n == "echo"),
        "echo must be gone after plugin.unload: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "claude-code"),
        "claude-code must remain after unloading echo: {names:?}"
    );
}

#[test]
#[cfg(unix)]
fn plugin_unload_refuses_when_adapter_is_bound() {
    // Load echo, start an adapter against it, then try to unload — the
    // sibling adapter_plugin map should report the binding without
    // touching the per-adapter Mutex<ExtensionEntry>, so the unload must
    // be refused even while the adapter is still alive.
    let manifest_path = echo_plugin_manifest_path();
    let manifest_str = manifest_path.to_str().expect("manifest path is utf8");
    let requests = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.load\",\"params\":{{\"manifest_path\":{path:?}}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"adapter.start\",\"params\":{{\"plugin\":\"echo\"}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"plugin.unload\",\"params\":{{\"plugin\":\"echo\"}}}}\n",
        ),
        path = manifest_str,
    );
    let mut child = bin()
        .args(["serve", "--stdio", "--allow-plugin-load"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --allow-plugin-load");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(requests.as_bytes())
        .expect("write requests");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert_eq!(responses[0]["result"]["plugin"], "echo");
    assert!(
        responses[1]["result"]["adapter"].is_string(),
        "adapter.start must succeed: {response:?}",
        response = responses[1]
    );
    assert_eq!(
        responses[2]["error"]["code"],
        -32602,
        "plugin.unload while live adapters bound must return InvalidParams: {response:?}",
        response = responses[2]
    );
    let message = responses[2]["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("live adapters"),
        "error should mention the live-adapter binding: {message}"
    );
}

#[test]
#[cfg(unix)]
fn plugin_load_allowed_with_flag_drives_full_lifecycle() {
    let manifest_path = echo_plugin_manifest_path();
    let manifest_str = manifest_path.to_str().expect("manifest path is utf8");
    let requests = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"plugin.load\",\"params\":{{\"manifest_path\":{path:?}}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"adapter.start\",\"params\":{{\"plugin\":\"echo\"}}}}\n",
        ),
        path = manifest_str,
    );
    let mut child = bin()
        .args(["serve", "--stdio", "--allow-plugin-load"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright serve --allow-plugin-load");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(requests.as_bytes())
        .expect("write requests");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ptywright serve");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert!(
        responses.len() >= 2,
        "expected at least two responses (plugin.load + adapter.start), got: {responses:?}"
    );
    assert_eq!(responses[0]["result"]["plugin"], "echo");
    assert_eq!(
        responses[1]["result"]["plugin"],
        "echo",
        "adapter.start must instantiate the freshly-loaded echo plugin: {response:?}",
        response = responses[1]
    );
    assert!(
        responses[1]["result"]["adapter"].is_string(),
        "adapter.start must allocate an adapter id: {response:?}",
        response = responses[1]
    );
}

#[test]
#[cfg(unix)]
fn logs_tail_streams_recent_lines_from_newest_file() {
    // Run something that writes to the log file (any subcommand
    // honours PTYWRIGHT_HOME and writes via the logging stack), then
    // run `logs --lines 100` to verify the tail subcommand finds the
    // file, prints the header, and includes the recent lines.
    use std::io::Write;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!("ptywright-logs-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&home).expect("create home");

    // Run any subcommand that writes a log file. `run` with a tiny
    // shell command does the job and exits fast.
    let preflight = bin()
        .env("PTYWRIGHT_HOME", &home)
        .env("PTYWRIGHT_LOG", "info")
        .args(["run", "--", "/bin/sh", "-lc", "printf logs-tail-test"])
        .output()
        .expect("preflight run");
    assert!(
        preflight.status.success(),
        "preflight failed: {preflight:?}"
    );

    let logs_dir = home.join("logs");
    assert!(logs_dir.exists(), "logs dir must exist after preflight");

    // Append a known marker line directly to the newest log file so
    // we can assert the tail captured it. Avoids racing against
    // tracing's async writer for the preflight's own entries.
    let mut entries: Vec<_> = std::fs::read_dir(&logs_dir)
        .expect("read logs")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ptywright."))
        })
        .collect();
    entries.sort();
    let newest = entries.pop().expect("at least one log file");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&newest)
        .expect("open newest log");
    writeln!(file, "TEST-MARKER-line-XYZ").expect("write marker");
    drop(file);

    // Spawn `ptywright logs` in the background; let it print the tail
    // and capture stdout for ~1 second, then kill it.
    let mut child = bin()
        .env("PTYWRIGHT_HOME", &home)
        .args(["logs", "--lines", "200"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ptywright logs");

    std::thread::sleep(Duration::from_millis(800));
    child.kill().expect("kill ptywright logs");
    let output = child
        .wait_with_output()
        .expect("collect ptywright logs output");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains("==> tailing"),
        "expected tail header; got:\n{stdout}"
    );
    assert!(
        stdout.contains("TEST-MARKER-line-XYZ"),
        "expected the marker we wrote to be in the tail; got:\n{stdout}"
    );
}

#[test]
#[cfg(unix)]
fn logs_command_errors_when_no_logs_exist() {
    // No prior `ptywright` invocation under this fresh home → no
    // log files. The subcommand should fail with a clear message
    // rather than panicking or hanging on an empty tail.
    use std::time::{SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home =
        std::env::temp_dir().join(format!("ptywright-nologs-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(home.join("logs")).expect("create logs dir");

    let output = bin()
        .env("PTYWRIGHT_HOME", &home)
        .args(["logs"])
        .output()
        .expect("run ptywright logs");
    assert!(
        !output.status.success(),
        "logs against an empty home should fail; got: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        stderr.contains("no ptywright log files"),
        "expected a clear error; got:\n{stderr}"
    );
}
