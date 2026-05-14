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

- Parse terminal output into screen snapshots.
- Track cursor, alternate screen, and scrollback where possible.
- Provide transcript-friendly debug output.

## Orchestration

- Wait for screen/output matchers.
- Model turn boundaries.
- Return structured success, timeout, and failure results.

## Protocol and CLI ergonomics

- JSON-RPC over stdio for external automation clients.
- Shell completion generation for bash, zsh, fish, elvish, and PowerShell.
- Plugin manifests and host capability reporting.

## Adapters

- Shell and REPL helpers.
- Full-screen TUI helpers.
- App-specific adapters that live outside the generic core.
