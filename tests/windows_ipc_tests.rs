//! End-to-end test for the Windows named-pipe IPC transport (GH #19).
//!
//! `ptywright serve --socket \\.\pipe\…` compiles on Windows via the
//! `interprocess` crate but was not exercised by any test before this file
//! landed. A regression in the Windows-only code paths in `src/main.rs` and
//! `src/rpc.rs` could therefore land silently against the cargo CI matrix.
//!
//! This file is gated on `#[cfg(windows)]` at the module level so non-Windows
//! builds never parse it.

#![cfg(windows)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::{GenericFilePath, Stream as LocalSocketStream, prelude::*};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ptywright"))
}

#[test]
fn serve_named_pipe_returns_json_rpc_response() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pipe_path = format!(r"\\.\pipe\ptywright-test-{}-{unique}", std::process::id());

    let mut child = bin()
        .args(["serve", "--socket"])
        .arg(&pipe_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptywright serve --socket");

    // Mirror the Unix variant: poll-connect until the listener is bound.
    let mut stream = None;
    for _ in 0..200 {
        let name = pipe_path
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .expect("pipe name");
        match LocalSocketStream::connect(name) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let mut stream = stream.expect("connect to named pipe");

    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.list\"}\n")
        .expect("write request");
    stream.flush().expect("flush");

    // The server keeps the pipe open per-connection until the client side
    // closes. Read one NDJSON line then drop the stream.
    let mut buf = [0_u8; 4096];
    let mut accumulated = Vec::new();
    for _ in 0..50 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                accumulated.extend_from_slice(&buf[..n]);
                if accumulated.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    let response_text = String::from_utf8(accumulated).expect("response is utf8");
    let line = response_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("at least one NDJSON line in response");
    let response: serde_json::Value = serde_json::from_str(line).expect("json response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(
        response["result"]["sessions"],
        serde_json::json!([]),
        "session.list on a freshly-spawned server must return an empty array: {response}"
    );
}
