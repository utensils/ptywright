# ptywright

[![CI](https://github.com/utensils/ptywright/actions/workflows/ci.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/ci.yml)
[![Deploy Docs](https://github.com/utensils/ptywright/actions/workflows/pages.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/pages.yml)
[![codecov](https://codecov.io/gh/utensils/ptywright/graph/badge.svg)](https://codecov.io/gh/utensils/ptywright)

**A cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.**

ptywright is an early general-purpose PTY/TUI automation toolkit. It is designed to drive interactive terminal applications from code without coupling the core abstractions to any one program.

The library now includes initial target, session, screen, action, matcher, transcript, and JSON-RPC primitives backed by real PTYs. The CLI includes `run` for executing a program in a headless PTY and `serve --stdio` for NDJSON-framed JSON-RPC automation.

Docs: <https://utensils.io/ptywright/>

## Quickstart

```bash
ptywright --help
ptywright --version
ptywright run -- /bin/sh -lc 'printf ready'
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | ptywright serve --stdio
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
7. JSON-RPC over stdio for external automation clients.

## Planned layers

1. Turn orchestration.
2. Interactive Claude Code adapter built on the generic PTY layers.
3. Extension/plugin APIs for trusted local adapters.

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
