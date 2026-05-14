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

`Session` owns a PTY-backed child process and keeps terminal state up to date from a background reader thread.

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
| `send`         | Apply an `Action`.                                 |
| `write_text`   | Write text bytes to the PTY.                       |
| `send_key`     | Send a named key sequence.                         |
| `resize`       | Resize the PTY and screen parser.                  |
| `wait_for`     | Wait for a `Matcher` with a timeout.               |
| `wait`         | Wait for child process exit.                       |
| `kill`         | Kill the child process.                            |

## Screen snapshots

`ScreenSnapshot` is the automation-friendly terminal view:

```rust
pub struct ScreenSnapshot {
    pub size: TerminalSize,
    pub cursor: CursorState,
    pub sequence: u64,
    pub plain_text: String,
}
```

The `sequence` increments as PTY output is processed or the terminal is resized. Matchers report the sequence they observed.

## Actions and keys

```rust
use ptywright::{Action, Key};

session.send(Action::Text("help\n".into()))?;
session.send(Action::Key(Key::Enter))?;
session.send(Action::Interrupt)?;
# Ok::<(), ptywright::Error>(())
```

Initial key support includes Enter, Escape, Tab, Backspace, arrows, Ctrl-C, and Ctrl-D.

## Matchers

`Matcher` evaluates screen snapshots and transcript tails:

- `ContainsText`
- `ScreenRegex`
- `TranscriptContains`
- `TranscriptRegex`
- `CursorAt`
- `Any`
- `All`

`wait_for` returns `MatchResult` with elapsed time, final snapshot, transcript tail, and sequence evidence.

## Planned API families

Next public APIs should grow around these reusable concepts:

- JSON-RPC server over stdio for external automation clients.
- Turn orchestration for request/response workflows.
- Interactive Claude Code adapter built on the generic session/screen/action/matcher layers.
- Extension/plugin host APIs for trusted local adapters.
