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
        "A Rust CLI and library for driving interactive terminal applications through PTYs"
    ));
    assert!(stdout.contains("Usage: ptywright"));
    assert!(stdout.contains("--version"));
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
