# ptywright — Agent Instructions

ptywright is a Rust CLI and library for driving interactive terminal applications through PTYs. The project is intentionally early, but the direction is a cross-platform automation toolkit with a generic core and application-specific adapters layered above it.

## Product intent

Build a local, scriptable driver for terminal software:

- Spawn or attach to PTY-backed processes.
- Observe terminal state as a user would see it.
- Send deterministic input actions.
- Wait for prompts, screen states, turn boundaries, and failures.
- Layer shell, REPL, full-screen TUI, and app-specific adapters above reusable primitives.

The core must stay generic. Do not bake a single target application into core names, traits, modules, or docs unless the code is explicitly adapter-specific.

## Abstraction rules

Prefer small, explicit layers:

1. **Target** — executable, arguments, environment, cwd, attach/spawn intent.
2. **Session** — PTY process lifecycle, dimensions, signals, exit status.
3. **Screen** — terminal parser output, cursor state, scrollback, alternate screen.
4. **Action** — key sequences, paste/write, resize, waits, interrupts.
5. **Matcher** — prompt detection, output predicates, timeout policies, error states.
6. **Turn** — request/response orchestration and transcript capture.
7. **Adapter** — app-specific workflows implemented over the generic layers.

Guidelines:

- Keep public types named after terminal automation concepts, not current demos.
- Prefer traits only when there are at least two realistic implementations or a clear testing boundary.
- Prefer plain structs/enums for data models and command configuration.
- Keep async/runtime choices isolated so the library can evolve without rewriting callers.
- Expose library APIs first; keep CLI commands thin and scriptable.
- Make new behavior observable and testable through transcripts, screen snapshots, or explicit result types.

## TDD requirements

Use test-driven development for behavior changes wherever practical:

- Add or update a failing unit/integration test before implementing new behavior.
- Prefer tests around public primitives: `Target`, `Session`, `Screen`, `Action`, `Matcher`, transcripts, CLI behavior, and future RPC methods.
- For PTY behavior, use deterministic fixture commands and platform-aware test helpers instead of sleeps or host-specific shell assumptions.
- Keep tests cross-platform unless a test is explicitly gated with `#[cfg(...)]` and the limitation is documented.
- Do not mark a milestone complete until tests and docs for that milestone are updated.

## Cross-platform requirements

ptywright should target macOS, Linux, and Windows.

- Avoid Unix-only APIs in public abstractions unless they are behind platform-specific modules or feature gates.
- Keep path, newline, shell invocation, and executable-extension behavior platform-aware.
- CI should exercise at least Linux, macOS, and Windows for core Rust checks.
- Nix support is for Unix development and packaging; Windows support should rely on Cargo/GitHub Actions release artifacts.
- Document platform limitations early rather than hiding them in implementation details.

## Build and development

Prefer Nix on macOS/Linux:

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

- `src/main.rs` — clap CLI. With no args it prints help; `--version` prints package version; `run` executes a command in a headless PTY and prints the captured transcript; `serve --stdio` exposes JSON-RPC; `completions` generates shell completions.
- `src/lib.rs` plus modules in `src/` — public library surface for target configuration, PTY sessions, screen snapshots, actions, matchers, transcripts, JSON-RPC, Claude Code adapter primitives, and plugin manifests.
- `tests/cli_tests.rs` — end-to-end checks for help/version output, basic PTY command execution, JSON-RPC stdio, and completions.
- `website/` — VitePress docs site.
- `.github/workflows/` — CI, docs deploy, and release packaging.
- `flake.nix` — Nix package, app, formatter, and devshell.

## Dependency upkeep

When asked to update dependencies:

```bash
cargo update
cd website && bun update
nix flake update
```

Use an authenticated GitHub token for `nix flake update` if unauthenticated API rate limits are hit:

```bash
TOKEN=$(gh auth token)
NIX_CONFIG="access-tokens = github.com=$TOKEN" nix flake update
```

## Documentation rules

- Keep docs current when changing public behavior.
- Document implemented PTY/session behavior as it lands.
- Mark planned capabilities as planned; do not imply unimplemented live bridging, embedded plugin runtimes, or high-fidelity Claude Code turn detection works today.
- Keep install and release docs honest about platform support.

## Conventions

- Use Conventional Commits.
- Prefer small, direct Rust modules with explicit types.
- Keep the CLI boring and composable.
- Prefer minimal diffs that fit the existing project style.
- Run `ci-local` or the direct equivalent before handing off substantial changes.
