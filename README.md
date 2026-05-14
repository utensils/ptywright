# ptywright

[![CI](https://github.com/utensils/ptywright/actions/workflows/ci.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/ci.yml)
[![Deploy Docs](https://github.com/utensils/ptywright/actions/workflows/pages.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/pages.yml)
[![codecov](https://codecov.io/gh/utensils/ptywright/graph/badge.svg)](https://codecov.io/gh/utensils/ptywright)

**A cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.**

ptywright is an early general-purpose PTY/TUI automation toolkit. It is designed to drive interactive terminal applications from code without coupling the core abstractions to any one program.

The library now includes initial target, session, rich screen snapshot, action, temporal matcher, transcript, redaction, JSON-RPC, a Lua-backed interactive Claude Code adapter, plugin manifest/runtime primitives, and shell completion primitives backed by real PTYs. The CLI includes `run` for live stdin/stdout PTY debugging, `serve --stdio` for NDJSON or LSP-style JSON-RPC automation, Unix socket serving on macOS/Linux, and `completions` for shell setup.

Docs: <https://utensils.io/ptywright/>

## Quickstart

```bash
ptywright --help
ptywright --version
ptywright run -- /bin/sh -lc 'printf ready'
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | ptywright serve --stdio
ptywright serve --stdio --framing lsp
ptywright serve --socket /tmp/ptywright.sock
source <(ptywright completions zsh)
```

## Install from source

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
nix develop
cargo build --release
./target/release/ptywright --help
```

Or run through Nix on macOS/Linux:

```bash
nix run github:utensils/ptywright -- --help
```

## Project goals

- Provide Rust abstractions for spawning, attaching to, and driving PTY-backed terminal applications.
- Keep application-specific adapters separate from the core architecture.
- Support deterministic turn execution, transcript capture, prompt detection, and output parsing.
- Target macOS, Linux, and Windows.
- Keep a clean CLI surface while exposing reusable library primitives.
- Preserve a small, auditable, local-first implementation.

## Current layers

1. Target configuration.
2. PTY session lifecycle.
3. Terminal screen observation.
4. Input actions and key sequences.
5. Matchers and waits.
6. Bounded transcript capture.
7. JSON-RPC over stdio or Unix sockets for external automation clients, with NDJSON and LSP-style framing.
8. Interactive Claude Code adapter built on the generic PTY layers with Claude-specific logic in a built-in Lua plugin.
9. Plugin manifests, permission declarations, and trusted embedded Lua runtime for adapter orchestration.
10. Shell completion generation for bash, zsh, fish, elvish, and PowerShell.
11. Rich screen snapshots with cell/style/mode metadata.
12. Redaction helpers and default RPC redaction for sensitive-looking output.

## Planned layers

1. Turn orchestration refinements.
2. Third-party plugin loading and stronger runtime isolation.

## Development

```bash
nix develop        # auto via direnv + use_flake
ci-local           # fmt-check → check → clippy → test → build
docs-dev           # run the VitePress docs site
```

Useful direct commands:

```bash
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked -- -D warnings
cargo test --locked
cargo run -- --help
```

See [AGENTS.md](AGENTS.md) for repository guidance.

## License

MIT — see [LICENSE](LICENSE).
