# Reference

ptywright exposes an early CLI, Rust library, JSON-RPC automation protocol, trusted Lua plugin runtime, and shell completion generation.

## Crate metadata

- Crate: `ptywright`
- Binary: `ptywright`
- Current version: `0.3.0`
- License: MIT
- Repository: <https://github.com/utensils/ptywright>
- Docs site: <https://utensils.io/ptywright/>

## Current public API families

- `Target` and `TerminalSize` for spawn configuration.
- `Session`, `SessionConfig`, `SessionEvent`, `SessionExitStatus`, `CancellationToken`, and `Signal` for PTY-backed process lifecycle (including cancellable waits and cross-platform signal delivery).
- `Terminal`, `ScreenSnapshot`, `ScreenCell`, `ScreenCellStyle`, and `CursorState` for rendered terminal observation.
- `Action` and `Key` for deterministic input (including `Action::MarkTranscript` for plugin-driven transcript boundary marks).
- `Matcher`, `MatchResult`, `MatchOutcome`, `MatcherContext`, `PluginRegistry`, `PredicateContext`, and `PredicateOutcome` for event-driven waits and plugin-defined `Matcher::Lua` predicates.
- `Transcript`, `TranscriptConfig`, and `TranscriptFileConfig` for bounded output retention with optional raw file streaming.
- `RpcServer`, `RpcServerState`, `serve_ndjson`, `serve_lsp`, `serve_ndjson_with_state`, and `serve_lsp_with_state` for JSON-RPC automation.
- `Extension`, `LuaExtension`, `ExtensionHandle`, `ExtensionStateSnapshot`, `ExtensionEvent`, `ActionPlan`, `ClassifyContext`, `HostMark`, and `StateCandidate` for the generic plugin-backed automation layer that drives Claude Code and any future built-in TUI plugin.
- `LuaPlugin`, `LuaPluginRegistry`, `PluginManifest`, `PluginManifestError`, `DefaultTarget`, `PluginKind`, `PluginRuntime`, `PluginPermission`, `PluginHostCapabilities`, and `builtin_manifests` for trusted extension authoring and host capability discovery.
- `RedactionPolicy` for opt-in caller-supplied redaction and `RedactingMakeWriter` / `RedactingWriter` for redaction-aware `tracing` sinks.
- `Paths` and `expand_tilde` for the `~/.ptywright/` runtime directory layout.
- `Config`, `LoggingConfig`, and `LogFormat` for on-disk configuration.
- `init_for_run`, `init_for_serve_stdio`, `init_for_serve_socket`, `init_for_oneshot`, `cleanup_old_logs`, and `LogGuard` for the structured-logging stack with daily rotation, retention, and per-record redaction.

See the dedicated CLI, library, JSON-RPC, plugin reference, and Claude Code guide pages for details.
