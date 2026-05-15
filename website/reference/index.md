# Reference

ptywright exposes an early CLI, Rust library, JSON-RPC automation protocol, trusted Lua plugin runtime, and shell completion generation.

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
- `Extension`, `LuaExtension`, `ExtensionHandle`, `ExtensionStateSnapshot`, and `ClassifyContext` for the generic plugin-backed automation layer that drives Claude Code and any future built-in TUI plugin.
- `LuaPlugin`, `PluginManifest`, `DefaultTarget`, `PluginRuntime`, `PluginPermission`, `builtin_manifests`, and host capability types for trusted extension planning.
- `Paths` and `expand_tilde` for the `~/.ptywright/` runtime directory layout.
- `Config`, `LoggingConfig`, and `LogFormat` for on-disk configuration.
- `init_for_run`, `init_for_serve_stdio`, `init_for_serve_socket`, `init_for_oneshot`, and `LogGuard` for the structured-logging stack with daily rotation, retention, and per-record redaction.

See the dedicated CLI, library, JSON-RPC, plugin reference, and Claude Code guide pages for details.
