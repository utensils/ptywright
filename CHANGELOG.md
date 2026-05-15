# Changelog

All notable changes to ptywright will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial ptywright skeleton.
- Minimal Rust CLI that prints help by default and supports `--version`.
- Minimal library surface for future PTY/TUI automation abstractions.
- Nix flake, devshell, GitHub CI, release packaging, install script, and VitePress docs.
- Integration tests for the `claude.approve`, `claude.deny`, and `claude.cancel` JSON-RPC dispatch paths and their `InvalidParams` error responses.
- End-to-end test that a `tracing` event with a secret-shaped structured field is masked through the full `RedactingMakeWriter` pipeline before reaching the underlying writer.
- Unix-gated test asserting raw transcript files are created with `0o600` permissions, matching SPEC's "restrictive permissions where supported" guarantee.
- `Action::BracketedPaste(String)` variant (and matching `action.bracketed_paste(...)` Lua host helper) for receivers that have enabled bracketed paste (Claude Code v2.1+, vim, fish, …). The Claude Code plugin's `send_prompt` uses this so a trailing Enter is interpreted as a submit instead of absorbed into the paste tokeniser.
- `dismiss_welcome` intent on the Claude Code adapter for clearing the first-launch welcome panel without dropping to `session.input`.
- LSP back-to-back framing and `session.changed` notification integration tests in `tests/cli_tests.rs`.
- `PluginManifest::default_target` (optional `{ program, args }`) for plugins to declare a sensible default spawn target. `adapter.start` falls back to this when callers omit `program`.
- `BUILTIN_PLUGINS` registry (a `&[BuiltinPlugin { manifest, source }]` slice) plus a thin `builtin_manifests()` accessor over it. Adding a new built-in Lua plugin is a single struct-literal entry — the manifest constructor and embedded Lua source travel together, replacing the previous hardcoded match arm in `LuaExtension::built_in`.
- `tests/lua_classifier_tests.rs` and `tests/lua_plugin_intents.rs` integration tests covering the claude-code Lua plugin through the generic `Extension` / `ExtensionHandle` surface.

### Changed

- Documented every `session.*` JSON-RPC method (`snapshot`, `transcript`, `resize`, `kill`, `close`) with full param tables and example responses, replacing the previous one-line table summary.
- Strengthened the JSON-RPC reference's redaction notes to call out the on-disk `0o600` mode used when creating new raw transcript files on Unix, and to explain that the embedded `RedactionPolicy.enabled` field is overridden server-side.
- `ExtensionHandle::wait` coerces `Value::Null` matcher params to an empty object and auto-injects the host's configured `completed_turn_stable_ms` so generic `adapter.wait` callers no longer have to know about plugin-side stability thresholds.
- `ExtensionHandle::send` runs the same Null-to-object coercion so generic `adapter.send` calls with no `params` no longer crash mlua on intents that index into `input.*`.
- Claude Code Lua plugin recognises the Claude Code 2.1.x workspace-trust dialog (`Accessing workspace…` / `Yes, I trust this folder`) for both classification and `wait_turn_matcher` wake-up, classifies the first-launch welcome panel as `starting` rather than `waiting_for_user_input`, detects the rotating thinking spinner (`<glyph> <Verb>…`), and reports `cancelling` for the brief window between Ctrl-C and the screen settling.
- Confidence fields serialise with 3-decimal precision so `0.62` shows up as `0.62` instead of `0.6200000047683716` on the JSON-RPC wire.

### Removed

- **Breaking:** the `claude.*` JSON-RPC method surface (`claude.start`, `claude.send_prompt`, `claude.wait_turn`, `claude.approve`, `claude.deny`, `claude.cancel`, `claude.state`, `claude.snapshot`, `claude.transcript`, `claude.inspect`). Callers must use the generic `adapter.*` surface instead. `adapter.start` accepts `{"plugin": "claude-code"}` and reads the manifest's `default_target` so the program does not need to be passed explicitly; `adapter.send` accepts `{"intent": "send_prompt"|"approve"|"deny"|"cancel"|...}` for every mutation that `claude.*` previously exposed.
- **Breaking:** the typed Rust adapter shim (`ClaudeCodeAdapter`, `ClaudeCodeConfig`, `ClaudeCodeState`, `ClaudeCodeStateSnapshot`) and the entire `src/adapters/` module. Rust callers should drive plugins through `ExtensionHandle` + `LuaExtension::built_in(<name>)` and read state strings off `ExtensionStateSnapshot` directly.
- `claude_code_manifest` is no longer part of the public library surface; use `builtin_manifests()` to enumerate registered plugins.

[Unreleased]: https://github.com/utensils/ptywright/compare/v0.1.0...HEAD
