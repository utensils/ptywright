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

| Field     | Type                   | Meaning                          |
| --------- | ---------------------- | -------------------------------- |
| `program` | `String`               | Executable name or path.         |
| `args`    | `Vec<String>`          | Arguments passed to the child.   |
| `cwd`     | `Option<PathBuf>`      | Optional working directory.      |
| `env`     | `BTreeMap<String,...>` | Environment overrides.           |
| `size`    | `TerminalSize`         | Initial rows/columns and pixels. |

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

| Method         | Purpose                                            |
| -------------- | -------------------------------------------------- |
| `spawn`        | Spawn from `SessionConfig`.                        |
| `spawn_target` | Spawn a `Target` with default transcript settings. |
| `snapshot`     | Return the current rendered screen snapshot.       |
| `transcript`   | Return retained transcript text.                   |
| `is_finished`  | Report whether the reader/session lifecycle ended. |
| `send`         | Apply an `Action`.                                 |
| `write_text`   | Write text bytes to the PTY.                       |
| `send_key`     | Send a named key sequence.                         |
| `resize`       | Resize the PTY and screen parser.                  |
| `wait_for`     | Wait for a `Matcher` with a timeout.               |
| `wait`         | Wait for child process exit.                       |
| `kill`         | Kill the child process.                            |

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
use ptywright::{Action, Key};

session.send(Action::Text("help\n".into()))?;
session.send(Action::Key(Key::Enter))?;
session.send(Action::Interrupt)?;
# Ok::<(), ptywright::Error>(())
```

Initial key support includes Enter, Escape, Tab, Backspace, arrows, Ctrl-C, and Ctrl-D. `Action`, `Key`, `Matcher`, `Target`, `TerminalSize`, and `ScreenSnapshot` are serializable for JSON-RPC use.

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

## Claude Code adapter

`ClaudeCodeAdapter` starts `claude` interactively in a PTY and classifies coarse TUI state from screen/transcript evidence. The adapter does not use `claude -p`. Claude-specific action plans, wait matchers, and classification rules live in the built-in Lua plugin at `plugins/claude-code/main.lua`; Rust executes the generic PTY controls.

```rust
use ptywright::{ClaudeCodeAdapter, ClaudeCodeConfig};

let mut claude = ClaudeCodeAdapter::start(ClaudeCodeConfig::default())?;
let state = claude.send_prompt("help")?;
# Ok::<(), ptywright::Error>(())
```

`ClaudeCodeAdapter::from_session(session)` is also fallible because it loads the built-in Lua plugin before wrapping externally managed sessions.

See [Claude Code adapter](../guide/claude-code.md) for state and limitation details.

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

| Function                  | Sinks                | When to use                                                |
| ------------------------- | -------------------- | ---------------------------------------------------------- |
| `init_for_run`            | file only            | Tools that bridge a raw PTY to the user's terminal.        |
| `init_for_serve_stdio`    | file + stderr        | JSON-RPC servers that own stdout for protocol framing.     |
| `init_for_serve_socket`   | file + stderr        | Local-IPC servers (Unix sockets, Windows named pipes).     |
| `init_for_oneshot`        | stderr only          | Short-lived commands (`--help`, `--version`, completions). |

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
