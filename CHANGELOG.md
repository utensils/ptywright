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
- `adapter.start` and `adapter.inspect` responses include `"session"` — the id allocated for the adapter's underlying PTY. Combined with `server.set_notifications`, this lets clients correlate `session.changed` / `session.exited` events with the spawning adapter without an extra round-trip.
- `session.changed` and `session.exited` notifications now fire for adapter-spawned PTYs (previously only `session.create`-spawned sessions notified). Adapter session ids share the `s<n>` namespace with directly-created sessions.
- `ptywright repl` gained `session.attach("id")` / `session.attach("all")` / `session.live()` DSL forms and `:attach <id|all>` / `:live` meta shortcuts so a fresh REPL can adopt adapters that were started by another connection (tmux-style attach). `:attach <id>` auto-renders the adapter's current screen on attach. The startup banner prints a one-line hint when live adapters exist on the server, and `Tab` completion suggests `all` plus locally-known adapter ids inside `:attach`.
- `ptywright repl` now subscribes to `session.changed` / `session.exited` notifications by default and prints them inline above the prompt via reedline's `external_printer`. A 500 ms `server.capabilities` heartbeat keeps the server's per-connection notification pump warm so events from sibling connections actually flush to an idle REPL. The heartbeat call is read-only by design, so it cannot race with a concurrent `:notifications off`.
- `claude-code` plugin gained a generic `key` intent (forwarded by the REPL DSL's `send.key("…")`). Named keys (`enter`, `escape`, `tab`, `backspace`, arrow keys, `ctrl-c` / `ctrl-d` — hyphens and underscores both accepted) emit the matching `action.key`; anything else (`"y"`, `"n"`, single digits) falls through to `action.text` so quick acknowledgements work without dropping to `send.text`.
- Expanded the host's `Key` enum (and the `claude-code` plugin's `M.key` alias table) to cover the full conventional terminal key surface: `shift-tab` (back-tab `\x1b[Z`), the navigation cluster (`home`, `end`, `page-up`, `page-down`, `insert`, `delete`, `space`), every readline-style ctrl combo from `ctrl-a` through `ctrl-z` (except the four that alias other named keys — `ctrl-h` / `ctrl-i` / `ctrl-j` / `ctrl-m`), and `f1` through `f12`. The REPL's `send.key("<TAB>")` completer now lists these with the most common (`enter`, `escape`, `tab`, `shift-tab`, `backspace`, `delete`) at the top of the menu and the long tail (less-used ctrl combos, function keys, text-fallthrough single chars) trailing behind.
- `ptywright serve --socket` (Unix) now installs SIGINT / SIGTERM / SIGHUP handlers that unlink the listening socket before `_exit`, so a clean Ctrl-C no longer leaves a dead socket file behind. Stale-socket cleanup on startup is unchanged — `serve` already reclaims an unbound socket before re-binding. The REPL's "stale socket" error message now points the operator at `ptywright serve` rather than telling them to `rm` the file by hand.
- **Default-on `repl` Cargo feature — interactive REPL client (`ptywright repl`).** Connects to a running `ptywright serve --socket <path>` or spawns a child `--stdio` server. Renders a ratatui chrome (header, tab strip, live screen preview, scrollable history pane, syntax-highlighted input, footer) and drives the generic `adapter.*` JSON-RPC surface from a small friendly DSL: `session.spawn("…")`, `send.text("…")`, `wait(matches(r"…"))`, `wait(screen_stable(250ms))`, `transcript.snapshot()`, etc. Power users can fall through to `:rpc <method> {json}` for raw JSON-RPC. Tab completion, history (persisted in `~/.ptywright/repl-history`), and DSL syntax highlighting come from the `reedline` traits the REPL implements. The feature is on by default; pass `--no-default-features` to opt out of `reedline`, `ratatui`, `crossbeam-channel`, and `nu-ansi-term`.
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
