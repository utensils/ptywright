# What is ptywright?

ptywright is a Rust CLI and library project for driving interactive terminal applications through PTYs.

The goal is to make terminal automation feel local, inspectable, and deterministic while still interacting with real terminal programs. The core should be reusable across shells, REPLs, full-screen TUIs, and long-running processes.

## Current status

ptywright is pre-0.1 but the core PTY/automation layers are in place:

- Cross-platform PTY-backed `Session` with terminal screen snapshots, transcripts, deterministic actions, and event-driven matchers.
- JSON-RPC 2.0 server over stdio (NDJSON or LSP framing) and multi-client local IPC (Unix sockets on macOS/Linux, named pipes on Windows).
- `ptywright run` for live PTY bridging and `ptywright completions` for bash/zsh/fish/elvish/powershell.
- Trusted embedded Lua runtime with a built-in interactive Claude Code adapter.
- Per-user runtime directory at `~/.ptywright/` (override with `PTYWRIGHT_HOME`) for configuration and rotated log files.
- Structured logging through `tracing` with daily rotation, configurable retention, and built-in redaction of secret-shaped values.
- Linux, macOS, and Windows in the supported platform matrix.

::: tip Planned vs implemented
The docs describe the intended shape so the project can grow in the right direction. Pages call out planned capabilities explicitly when an API does not exist yet.
:::

## Design principles

- **General purpose first.** App-specific behavior belongs above the core PTY abstractions.
- **PTY-native.** Model terminal behavior as it appears to a real user.
- **Deterministic where possible.** Prefer explicit screen matching, turn boundaries, and captured transcripts.
- **Cross-platform by default.** Keep platform-specific APIs behind clear boundaries.
- **Small and local.** Avoid unnecessary daemons or cloud dependencies.
- **Library plus CLI.** Keep reusable Rust primitives under the crate and expose practical workflows through the binary.

## Planned layers

1. **Target** — what to spawn or attach to.
2. **Session** — PTY process lifecycle and terminal dimensions.
3. **Screen** — observed text, cursor state, alternate screen, and scrollback.
4. **Action** — keys, paste, resize, signals, and waits.
5. **Matcher** — prompts, status lines, completion markers, and error states.
6. **Turn** — request/response orchestration and transcript capture.
7. **Adapter** — application-specific workflows over the generic primitives.
