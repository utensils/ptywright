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

### Changed

- Documented every `session.*` JSON-RPC method (`snapshot`, `transcript`, `resize`, `kill`, `close`) with full param tables and example responses, replacing the previous one-line table summary.
- Strengthened the JSON-RPC reference's redaction notes to call out the on-disk `0o600` mode used when creating new raw transcript files on Unix, and to explain that the embedded `RedactionPolicy.enabled` field is overridden server-side.
- `ExtensionHandle::wait` coerces `Value::Null` matcher params to an empty object and auto-injects the host's configured `completed_turn_stable_ms` so generic `adapter.wait` callers no longer have to know about plugin-side stability thresholds.
- `ExtensionHandle::send` runs the same Null-to-object coercion so generic `adapter.send` calls with no `params` no longer crash mlua on intents that index into `input.*`.
- Claude Code Lua plugin recognises the Claude Code 2.1.x workspace-trust dialog (`Accessing workspace…` / `Yes, I trust this folder`) for both classification and `wait_turn_matcher` wake-up, classifies the first-launch welcome panel as `starting` rather than `waiting_for_user_input`, detects the rotating thinking spinner (`<glyph> <Verb>…`), and reports `cancelling` for the brief window between Ctrl-C and the screen settling.
- Confidence fields serialise with 3-decimal precision so `0.62` shows up as `0.62` instead of `0.6200000047683716` on the JSON-RPC wire.

[Unreleased]: https://github.com/utensils/ptywright/compare/v0.1.0...HEAD
