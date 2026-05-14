# ptywright — Agent Instructions

ptywright is a Rust CLI and library for driving interactive terminal applications through PTYs. It starts as a small skeleton with package, docs, Nix, CI, and release plumbing already in place. The product direction is general-purpose TUI automation with application-specific adapters layered above reusable PTY primitives.

## Build and development

Prefer Nix:

```bash
nix develop
ci-local
```

Devshell commands:

| Category | Command | Description |
| --- | --- | --- |
| build | `build` / `build-release` | `cargo build` / `cargo build --release` |
| check | `check` / `clippy` / `fmt` / `fmt-check` | Standard Rust checks |
| check | `run-tests` | `cargo test` |
| check | `ci-local` | fmt-check → check → clippy → test → build |
| check | `coverage` | `cargo llvm-cov --workspace --summary-only` |
| run | `ptywright` | `cargo run -- "$@"` |
| docs | `docs-dev` / `docs-build` / `docs-preview` | VitePress documentation |

Direct Cargo fallback:

```bash
cargo fmt --all -- --check
cargo check
cargo clippy -- -D warnings
cargo test
cargo run -- --help
```

## Current architecture

- `src/main.rs` — clap CLI. With no args it prints help; `--version` prints package version.
- `src/lib.rs` — minimal public library surface and placeholder abstractions.
- `tests/cli_tests.rs` — end-to-end checks for help/version output.
- `website/` — VitePress docs site.
- `.github/workflows/` — CI, docs deploy, and release packaging.
- `flake.nix` — Nix package, app, formatter, and devshell.

## Direction

Keep the core generic. Avoid naming abstractions after one target application unless they are adapter-specific. Prefer layers like:

1. PTY process/session management.
2. Terminal screen observation.
3. Input actions and key sequences.
4. Prompt/turn orchestration.
5. App-specific adapters for shells, REPLs, full-screen TUIs, and other terminal applications.

## Conventions

- Use Conventional Commits.
- Prefer small, direct Rust modules with explicit types.
- Keep the CLI boring and scriptable.
- Keep docs current when changing public behavior.
- Run `ci-local` before handing off substantial changes.
