use std::io::Write;
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
