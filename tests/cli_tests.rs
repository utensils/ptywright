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
