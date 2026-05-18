# Library reference

ptywright exposes early primitives for driving interactive terminal programs through PTYs. The API is intentionally small and may evolve while the project is pre-1.0.

## Constants

```rust
pub const NAME: &str;
pub const VERSION: &str;
pub const DESCRIPTION: &str;
```

These values are sourced from Cargo metadata.

## Target configuration

`Target` describes the program to spawn in a PTY.

```rust
use ptywright::{Target, TerminalSize};

let target = Target::new("python")
    .arg("-i")
    .env("TERM", "xterm-256color")
    .size(TerminalSize::new(40, 120));
```

Important fields:

| Field       | Type                   | Meaning                                                          |
| ----------- | ---------------------- | ---------------------------------------------------------------- |
| `program`   | `String`               | Executable name or path.                                         |
| `args`      | `Vec<String>`          | Arguments passed to the child.                                   |
| `cwd`       | `Option<PathBuf>`      | Optional working directory.                                      |
| `env`       | `BTreeMap<String,...>` | Environment overrides applied to the spawned child.              |
| `clear_env` | `bool`                 | When `true`, strip the parent environment before applying `env`. |
| `size`      | `TerminalSize`         | Initial rows/columns and pixels.                                 |

`Target::clear_env()` flips the strip-parent-env flag for callers consuming a manifest whose `default_target.required_env` declares safety-critical environment variables — without it, an inherited variable could shadow a manifest-mandated default. `Target::env_snapshot()` returns the overlay map (`&BTreeMap<String, String>`) without exposing the parent environment, which is useful for callers that want to detect env drift across a respawn.

## Sessions

`Session` owns a PTY-backed child process and keeps terminal state up to date from a background reader thread. Transcript retention is bounded in memory by default; callers can explicitly opt into raw transcript file streaming through `TranscriptFileConfig`.

```rust
use std::time::Duration;
use ptywright::{Matcher, Session, Target};

let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "printf ready"]))?;
session.wait_for(&Matcher::ContainsText("ready".into()), Duration::from_secs(5))?;
let snapshot = session.snapshot();
let transcript = session.transcript();
# Ok::<(), ptywright::Error>(())
```

Key methods:

| Method                 | Purpose                                                                               |
| ---------------------- | ------------------------------------------------------------------------------------- |
| `spawn`                | Spawn from `SessionConfig`.                                                           |
| `spawn_target`         | Spawn a `Target` with default transcript settings.                                    |
| `snapshot`             | Return the current rendered screen snapshot.                                          |
| `transcript`           | Return retained transcript text.                                                      |
| `is_finished`          | Report whether the reader/session lifecycle ended.                                    |
| `pid`                  | PID of the underlying child process, or `None` once it has exited.                    |
| `send`                 | Apply an `Action` (including `Action::Signal(Signal::...)` on supported platforms).   |
| `signal`               | Deliver a `Signal` directly (`Term`, `Hup`, `Quit`, `Int`, `Kill`, `User1`, `User2`). |
| `terminate`            | SIGTERM then escalate to SIGKILL after the supplied grace period.                     |
| `write_text`           | Write text bytes to the PTY.                                                          |
| `send_key`             | Send a named key sequence.                                                            |
| `resize`               | Resize the PTY and screen parser.                                                     |
| `wait_for`             | Wait for a `Matcher` with a timeout.                                                  |
| `wait_for_cancellable` | Same as `wait_for`, but aborts cleanly when a shared `CancellationToken` is flipped.  |
| `events`               | Subscribe to `SessionEvent` notifications (`Changed` / `Exited`).                     |
| `mark_transcript`      | Stamp a label-keyed marker at the current transcript cursor.                          |
| `transcript_marker`    | Recall a previously stamped marker's offset.                                          |
| `transcript_slice`     | Slice the transcript between two markers.                                             |
| `wait`                 | Wait for child process exit.                                                          |
| `kill`                 | Kill the child process.                                                               |

```rust
use std::time::Duration;
use ptywright::{Session, Signal, Target};

let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 5"]))?;
session.signal(Signal::Term)?;
// or: session.terminate(Duration::from_millis(200))?;
# Ok::<(), ptywright::Error>(())
```

`Signal` covers the cross-platform set used to coordinate graceful shutdown — `Term`, `Hup`, `Quit`, `Int`, `Kill`, `User1`, and `User2`. On Unix every variant maps to its `libc` constant. On Windows only `Term`, `Int`, and `Kill` are honored; the remaining variants return `Error::UnsupportedOnPlatform` so callers can fall back to a portable path. `Session::terminate(grace)` is the conventional escalation: SIGTERM, poll `try_wait()` until `grace` elapses, then SIGKILL.

### Events

```rust
use ptywright::{Session, SessionEvent, Target};

let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "printf ready"]))?;
let rx = session.events();
while let Ok(event) = rx.recv() {
    match event {
        SessionEvent::Changed { sequence } => println!("seq={sequence}"),
        SessionEvent::Exited(status) => {
            println!("exit={:?}", status);
            break;
        }
    }
}
# Ok::<(), ptywright::Error>(())
```

`Session::events()` returns an independent `mpsc::Receiver<SessionEvent>`; the session fans out each event to every active subscriber. Variants are intentionally minimal — `Changed` for sequence bumps and `Exited` for terminal exit — so the reader hot path stays cheap.

### Cancellable waits

```rust
use std::time::Duration;
use ptywright::{CancellationToken, Matcher, Session, Target};

let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 30"]))?;
let cancel = CancellationToken::new();
let stopper = {
    let cancel = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    })
};
let outcome = session.wait_for_cancellable(
    &Matcher::ContainsText("never".into()),
    Duration::from_secs(60),
    &cancel,
);
stopper.join().unwrap();
assert!(matches!(outcome, Err(ptywright::Error::Cancelled)));
# Ok::<(), ptywright::Error>(())
```

`CancellationToken` is a thin `Arc<AtomicBool>` wrapper. Clone it across threads and call `cancel()` to flip the flag — any in-progress `wait_for_cancellable` notices on its next poll tick and returns `Error::Cancelled`.

### Transcript markers

```rust
use ptywright::{Session, Target};

let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "echo hello; sleep 0.05; echo world"]))?;
let start = session.mark_transcript("turn_start");
// ... drive the session, wait for completion ...
let end = session.mark_transcript("turn_end");
let segment = session.transcript_slice(start, end);
println!("{}", segment.unwrap_or_default());
# Ok::<(), ptywright::Error>(())
```

Markers are label-keyed cursors into the bounded in-memory transcript. The transcript itself remains bounded by `transcript_max_chars`; markers store the absolute byte offset so `transcript_slice` can return `None` once the requested range has rolled out of the retained window. The marker table is capped at 64 entries — extra labels are dropped with a `tracing::warn!`.

```rust
use ptywright::{Session, SessionConfig, Target, TranscriptFileConfig};

let mut config = SessionConfig::new(Target::new("/bin/sh"));
config.transcript.raw_file = Some(TranscriptFileConfig::new("raw-session.log"));
let session = Session::spawn(config)?;
# session.kill()?;
# Ok::<(), ptywright::Error>(())
```

Raw transcript files are unredacted and explicit opt-in. By default ptywright creates a new file and refuses to overwrite it; use `TranscriptFileConfig::append(true)` only for trusted local workflows that intentionally append.

## Screen snapshots

`ScreenSnapshot` is the automation-friendly terminal view:

```rust
pub struct ScreenSnapshot {
    pub size: TerminalSize,
    pub cursor: CursorState,
    pub sequence: u64,
    pub plain_text: String,
    pub cells: Vec<ScreenCell>,
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub title: Option<String>,
}
```

The `sequence` increments as PTY output is processed or the terminal is resized. Matchers report the sequence they observed. `plain_text` remains the easiest matcher surface; `cells` expose row/column text, color/style flags, and wide-character metadata for adapters that need structured screen inspection. `title` is populated from terminal OSC title sequences when applications set one.

## Actions and keys

```rust
use ptywright::{Action, Key, Signal};

session.send(Action::Text("help\n".into()))?;
session.send(Action::Key(Key::Enter))?;
session.send(Action::Interrupt)?;
session.send(Action::Signal(Signal::Term))?;
# Ok::<(), ptywright::Error>(())
```

Initial key support includes Enter, Escape, Tab, Backspace, arrows, Ctrl-C, and Ctrl-D. `Action::Signal(Signal)` routes through `Session::signal` so callers can compose signal delivery into the same `Vec<Action>` plans plugins return. `Action`, `Key`, `Matcher`, `Signal`, `Target`, `TerminalSize`, and `ScreenSnapshot` are serializable for JSON-RPC use.

## Matchers

`Matcher` evaluates screen snapshots and transcript tails:

- `ContainsText`
- `ScreenRegex`
- `TranscriptContains`
- `TranscriptRegex`
- `CursorAt`
- `ScreenStable`
- `ProcessExited`
- `Any`
- `All`

`wait_for` returns `MatchResult` with elapsed time, final snapshot, transcript tail, and sequence evidence.

## Driving a TUI plugin

There is no per-application Rust shim. Application-specific behaviour lives in Lua plugins under `plugins/<name>/`, and Rust callers drive them through the generic `Extension` / `ExtensionHandle` surface. `LuaExtension::built_in(<name>)` loads the named built-in plugin; `ExtensionHandle::start` wraps it around a `Session`; `handle.send(intent, params)` dispatches plugin-defined intents and returns the post-apply state snapshot.

```rust
use serde_json::json;

use ptywright::{ExtensionHandle, LuaExtension, Session, SessionConfig, Target};

let target = Target::new("claude");
let session = Session::spawn(SessionConfig::new(target))?;
let extension = LuaExtension::built_in("claude-code")?;
let mut handle = ExtensionHandle::start(Box::new(extension), session, 300);

let state = handle.send("send_prompt", json!({ "prompt": "help" }))?;
println!("state={}, evidence={}", state.state, state.evidence);
# Ok::<(), ptywright::Error>(())
```

`state.state` is a plugin-defined string (e.g. `"prompt_submitted"`, `"thinking"`, `"completed_turn"`); the Rust core does not interpret it. The matching catalogue of state strings for the bundled claude-code plugin is documented in the [Claude Code adapter guide](../guide/claude-code.md).

For Rust callers that prefer a typed enum, define one locally and convert from the plugin's state string — that translation is application-specific and intentionally not part of the library surface.

### Atomic turns and event subscriptions

`ExtensionHandle::turn` is the atomic `send` + `wait` convenience — same dispatch as `send`, same matcher loop as `wait`, with the handle held across both legs so a competing call site can't slip an intent between them:

```rust
use std::time::Duration;
use serde_json::json;

# use ptywright::{ExtensionHandle, LuaExtension, Session, SessionConfig, Target};
# let target = Target::new("claude");
# let session = Session::spawn(SessionConfig::new(target))?;
# let extension = LuaExtension::built_in("claude-code")?;
# let mut handle = ExtensionHandle::start(Box::new(extension), session, 300);
let (state, outcome) = handle.turn(
    "send_prompt",
    json!({ "prompt": "help" }),
    None, // wait_intent — defaults to "wait_turn_matcher"
    json!({}),
    Duration::from_secs(120),
)?;
println!("state={}, matched={:?}", state.state, outcome);
# Ok::<(), ptywright::Error>(())
```

`ExtensionHandle::subscribe()` returns an `mpsc::Receiver<ExtensionEvent>` that yields `StateChanged(ExtensionStateSnapshot)` after every classify pass — useful for drivers that want event-driven state updates instead of polling `state()`. Each call returns an independent receiver; the host fans out events to every active subscriber.

```rust
use ptywright::ExtensionEvent;

# use ptywright::{ExtensionHandle, LuaExtension, Session, SessionConfig, Target};
# let target = Target::new("claude");
# let session = Session::spawn(SessionConfig::new(target))?;
# let extension = LuaExtension::built_in("claude-code")?;
# let mut handle = ExtensionHandle::start(Box::new(extension), session, 300);
let rx = handle.subscribe();
// ... drive the handle on another thread ...
if let Ok(ExtensionEvent::StateChanged(snapshot)) = rx.recv() {
    println!("state -> {}", snapshot.state);
}
# Ok::<(), ptywright::Error>(())
```

## Redaction

`RedactionPolicy` redacts sensitive-looking values such as `token=...`, `password=...`, bearer tokens, and common API key shapes from strings. Policies also accept trusted-local `extra_literals` and `extra_regexes` additions. RPC read methods redact transcript and snapshot text by default, while in-process `Session::transcript()` remains raw and `Session::redacted_transcript()` applies an explicit policy.

```rust
use ptywright::RedactionPolicy;

let safe = RedactionPolicy::default().redact("token=secret");
assert_eq!(safe, "token=[REDACTED]");
```

## JSON-RPC

`RpcServer` handles JSON-RPC messages, `RpcServerState` shares sessions across multiple connection handlers, `serve_ndjson` runs newline-delimited JSON framing over arbitrary `Read`/`Write` streams, and `serve_lsp` runs LSP-style `Content-Length` framing.

```rust
use ptywright::RpcServer;

let mut server = RpcServer::new();
let response = server.handle_line(
    r#"{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}"#,
)?;
assert!(response.is_some());
# Ok::<(), ptywright::Error>(())
```

The CLI exposes this through `ptywright serve --stdio`, `ptywright serve --stdio --framing lsp`, and multi-client local IPC via `ptywright serve --socket PATH` (Unix sockets on macOS/Linux and named pipes on Windows). `RpcServer::handle_line_messages` returns a direct response plus opt-in coalesced notifications when enabled with `server.set_notifications`; notification subscriptions are per `RpcServer` connection handler, while sessions can be shared through `RpcServerState`.

## Plugin manifests and Lua plugins

The extension surface is declarative and includes a trusted embedded Lua runtime for built-in adapter orchestration. `PluginManifest` records plugin kind, version, optional runtime/entrypoint, and explicit permissions; `PluginHostCapabilities` reports the permissions this build understands and built-in plugin manifests.

```rust
use ptywright::{PluginManifest, PluginPermission};

let manifest: PluginManifest = serde_json::from_str(r#"{
  "name":"demo",
  "kind":"adapter",
  "version":"0.1.0",
  "runtime":"lua",
  "entrypoint":"main.lua",
  "permissions":["session.spawn","screen.read"]
}"#)?;
manifest.validate().expect("valid manifest");
# Ok::<(), Box<dyn std::error::Error>>(())
```

`LuaPlugin` is available for trusted embedded plugins and is called only on explicit adapter/orchestration boundaries. PTY byte processing, terminal parsing, screen mutation, and matcher polling remain in Rust.

WASM and third-party plugin loading are not enabled yet.

## Runtime directory

`Paths` resolves the per-user `~/.ptywright/` runtime directory and exposes per-layout accessors. Resolution order: `PTYWRIGHT_HOME` env var → `~/.ptywright/` → `./.ptywright` (only if `HOME` is unset).

```rust
use ptywright::{Paths, expand_tilde};

let paths = Paths::from_env();
let logs = paths.ensure_logs_dir()?;
assert!(logs.ends_with("logs"));

let absolute = expand_tilde("~/projects/foo");
# Ok::<(), ptywright::Error>(())
```

Accessors: `home`, `config_path`, `logs_dir`, `data_dir`, `transcripts_dir`, `sockets_dir`. `Paths::ensure_dir(path)` lazily creates a directory tree on demand. `expand_tilde` is a pure helper for paths that come from config strings.

## Configuration

`Config` reads `~/.ptywright/config.toml` and merges with sensible defaults. Missing files, missing sections, and missing keys all fall back to documented defaults; unknown keys are tolerated for forward compatibility.

```rust
use ptywright::{Config, LogFormat, Paths};

let paths = Paths::from_env();
let config = Config::load_or_default(&paths.config_path())?;
assert!(config.logging.file);
assert_eq!(config.logging.format, LogFormat::Text);
# Ok::<(), ptywright::Error>(())
```

`LoggingConfig` fields: `level` (`EnvFilter` directive), `file` (write rotated log files), `max_days` (retention window; `0` disables cleanup), `format` (`Text` | `Json`).

## Logging

ptywright wires `tracing` differently per CLI mode so output contracts (stdout-only JSON-RPC, no stderr corruption during `run`) are upheld. Pick the right helper for your entrypoint and hold the returned `LogGuard` for the lifetime of the process so the non-blocking writer can flush at exit.

| Function                | Sinks         | When to use                                                |
| ----------------------- | ------------- | ---------------------------------------------------------- |
| `init_for_run`          | file only     | Tools that bridge a raw PTY to the user's terminal.        |
| `init_for_serve_stdio`  | file + stderr | JSON-RPC servers that own stdout for protocol framing.     |
| `init_for_serve_socket` | file + stderr | Local-IPC servers (Unix sockets, Windows named pipes).     |
| `init_for_oneshot`      | stderr only   | Short-lived commands (`--help`, `--version`, completions). |

```rust
use ptywright::{Config, Paths, init_for_serve_stdio};

let paths = Paths::from_env();
let config = Config::load_or_default(&paths.config_path())?;
let _log_guard = init_for_serve_stdio(&paths, &config.logging);
# Ok::<(), ptywright::Error>(())
```

`RedactingMakeWriter` wraps any `tracing-subscriber` `MakeWriter` so each formatted record is run through `RedactionPolicy` before reaching its sink. `cleanup_old_logs(dir, max_days)` is exposed for callers that want to trigger retention sweeps outside of the standard init flow.

## Planned API families

Next public APIs should grow around these reusable concepts:

- Turn orchestration refinements for request/response workflows.
- Extension/plugin host APIs for trusted local adapters.
