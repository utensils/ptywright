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

### Changed

- Documented every `session.*` JSON-RPC method (`snapshot`, `transcript`, `resize`, `kill`, `close`) with full param tables and example responses, replacing the previous one-line table summary.
- Strengthened the JSON-RPC reference's redaction notes to call out the on-disk `0o600` mode used when creating new raw transcript files on Unix, and to explain that the embedded `RedactionPolicy.enabled` field is overridden server-side.

[Unreleased]: https://github.com/utensils/ptywright/compare/v0.1.0...HEAD
