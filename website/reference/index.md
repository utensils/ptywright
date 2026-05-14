# Reference

ptywright exposes an early CLI, Rust library, and JSON-RPC automation protocol.

## Crate metadata

- Crate: `ptywright`
- Binary: `ptywright`
- Current version: `0.1.0`
- License: MIT
- Repository: <https://github.com/utensils/ptywright>
- Docs site: <https://utensils.io/ptywright/>

## Current public API families

- `Target` and `TerminalSize` for spawn configuration.
- `Session` and `SessionConfig` for PTY-backed process lifecycle.
- `Terminal`, `ScreenSnapshot`, and `CursorState` for rendered terminal observation.
- `Action` and `Key` for deterministic input.
- `Matcher` and `MatchResult` for event-driven waits.
- `Transcript` and `TranscriptConfig` for bounded output retention.
- `RpcServer` and `serve_ndjson` for JSON-RPC automation.
- `ClaudeCodeAdapter`, `ClaudeCodeConfig`, and Claude state types for interactive Claude Code automation.

See the dedicated CLI, library, JSON-RPC reference, and Claude Code guide pages for details.
