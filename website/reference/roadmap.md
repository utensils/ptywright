# Roadmap

ptywright is early-stage. This roadmap captures the intended shape without promising that all APIs exist today.

## Foundation

- Package metadata, CLI commands, tests, Nix flake, docs, CI, and release workflows.
- Cross-platform CI for Linux, macOS, and Windows.
- Documentation structure for install, architecture, platforms, and reference.

## Core PTY layer

- Spawn PTY-backed processes.
- Resize sessions.
- Write bytes and key sequences.
- Capture output and exit status.
- Abstract Unix PTYs and Windows ConPTY behind common types.

## Terminal observation

- Parse terminal output into rich screen snapshots.
- Track cursor, alternate screen, terminal modes, cells, styles, and scrollback where possible.
- Provide transcript-friendly debug output.

## Orchestration

- Wait for screen/output, stable-screen, and process-exit matchers.
- Model turn boundaries.
- Return structured success, timeout, and failure results.

## Protocol and CLI ergonomics

- JSON-RPC over stdio and Unix sockets for external automation clients with NDJSON and LSP-style framing.
- Opt-in coalesced session notifications.
- Shell completion generation for bash, zsh, fish, elvish, and PowerShell.
- Plugin manifests and host capability reporting.

## Adapters

- Shell and REPL helpers.
- Full-screen TUI helpers.
- App-specific adapters that live outside the generic core.

## Runtime and operations

- Per-user runtime directory at `~/.ptywright/` (override with `PTYWRIGHT_HOME`) for config, rotated logs, and reserved space for future cached state.
- Optional TOML config file with forgiving defaults and forward-compatible parsing.
- Structured logging through `tracing` with daily-rotated files, configurable retention, JSON or text formats, and built-in redaction of secret-shaped values on every record.
- Per-mode logging init helpers that uphold each subcommand's output contract (file-only for `run`, file + stderr for `serve`, stderr-only for one-shot commands).
