//! Tiny std-only TUI fixture used by the ptywright end-to-end integration test.
//!
//! Behaviour:
//!   1. Print a known prompt (`READY> `) and flush.
//!   2. Read stdin one line at a time:
//!      - Non-empty line: echo as `> <line>` and flush, then loop.
//!      - Empty line: print `<answer>OK</answer>` and exit cleanly.
//!   3. If stdin closes (EOF) before a blank line, print the answer marker and exit
//!      so the test does not hang on early termination.
//!
//! No external crates: `std` only. This binary is wired into the workspace
//! `Cargo.toml` as the `ptywright-echo-tui` bin target so `cargo test --locked`
//! builds it alongside the integration test.

use std::io::{self, BufRead, Write};

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "READY> ");
    let _ = out.flush();

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        match lines.next() {
            Some(Ok(line)) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    let _ = writeln!(out, "<answer>OK</answer>");
                    let _ = out.flush();
                    return;
                }
                let _ = writeln!(out, "> {trimmed}");
                let _ = out.flush();
            }
            Some(Err(_)) | None => {
                let _ = writeln!(out, "<answer>OK</answer>");
                let _ = out.flush();
                return;
            }
        }
    }
}
