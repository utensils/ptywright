# ptywright

[![CI](https://github.com/utensils/ptywright/actions/workflows/ci.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/ci.yml)
[![Deploy Docs](https://github.com/utensils/ptywright/actions/workflows/pages.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/pages.yml)
[![codecov](https://codecov.io/gh/utensils/ptywright/graph/badge.svg)](https://codecov.io/gh/utensils/ptywright)

**A Rust CLI and library for driving interactive terminal applications through PTYs.**

ptywright is a fresh skeleton for a general-purpose PTY/TUI automation toolkit. It is designed to drive interactive terminal applications from code without coupling the core abstractions to any one program.

The binary currently only prints help and version output. The project already includes the important plumbing: Cargo package metadata, Nix flake, devshell commands, GitHub CI, release workflow, install script, and VitePress documentation site.

## Quickstart

```bash
ptywright --help
ptywright --version
```

## Install from source

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
nix develop
cargo build --release
./target/release/ptywright --help
```

Or run through Nix:

```bash
nix run github:utensils/ptywright -- --help
```

## Project goals

- Provide Rust abstractions for spawning, attaching to, and driving PTY-backed terminal applications.
- Keep application-specific adapters separate from the core architecture.
- Support deterministic turn execution, transcript capture, prompt injection, and output parsing.
- Keep a clean CLI surface while exposing reusable library primitives.
- Preserve a small, auditable, local-first implementation.

## Development

```bash
nix develop        # auto via direnv + use_flake
ci-local           # fmt-check → check → clippy → test → build
docs-dev           # run the VitePress docs site
```

Useful direct commands:

```bash
cargo fmt --all -- --check
cargo check
cargo clippy -- -D warnings
cargo test
cargo run -- --help
```

See [AGENTS.md](AGENTS.md) for repository guidance.

## License

MIT — see [LICENSE](LICENSE).
