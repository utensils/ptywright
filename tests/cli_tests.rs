use std::process::Command;

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
