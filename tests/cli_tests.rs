use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ptywright"))
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
