# ptywright — Agent Instructions

`AGENTS.md` is the canonical agent guide for this repository; `CLAUDE.md` is a symlink to it. Edits to either file land in `AGENTS.md` — keep that in mind when opening it as `CLAUDE.md`.

ptywright is a Rust CLI and library for driving interactive terminal applications through PTYs. The project is intentionally early, but the direction is a cross-platform automation toolkit with a generic core and application-specific adapters layered above it.

## Product intent

Build a local, scriptable driver for terminal software:

- Spawn or attach to PTY-backed processes.
- Observe terminal state as a user would see it.
- Send deterministic input actions.
- Wait for prompts, screen states, turn boundaries, and failures.
- Layer shell, REPL, full-screen TUI, and app-specific adapters above reusable primitives.

The core must stay generic. Do not bake a single target application into core names, traits, modules, or docs unless the code is explicitly adapter-specific.

Long-form design notes, milestone history, and open questions may live in a local-only, git-ignored `SPEC.md`. If that file exists in the working tree, read it before making structural changes; do not assume it is present in fresh clones.

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
- When changing `claude_code` adapter behavior, regenerate or hand-update the relevant fixtures under `tests/fixtures/claude_code/` in the same PR and explain the diff (e.g. "captured against Claude Code <version>" or "manual edit to cover X transition") in the PR description.

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

Direct Cargo fallback (CI uses `--locked` for Cargo check/clippy/test jobs; mirror that when reproducing CI failures locally):

```bash
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked -- -D warnings
cargo test --locked
cargo run -- --help
```

Run a single test:

```bash
cargo test --test cli_tests <test_name>   # one integration test in tests/cli_tests.rs
cargo test <module>::<test_name>          # one unit test inside a src/ module
cargo test <substring> -- --nocapture     # filter by substring and show stdout
cargo test --doc                          # rustdoc examples in public APIs
```

The default build embeds Lua 5.4 via `mlua` with the `vendored` feature, so source builds outside the Nix devshell need a working C compiler. The Rust toolchain is pinned in `rust-toolchain.toml`; rustup will fetch the pinned channel automatically when building outside Nix.

## Current architecture

The codebase is organized so each generic abstraction layer lives in one focused module, and adapters are layered on top without leaking back into the core.

- `src/main.rs` — clap CLI wiring. With no args it prints help; `--version` prints package version; `serve --stdio` exposes JSON-RPC with NDJSON or LSP-style framing; `serve --socket` exposes Unix sockets on macOS/Linux and named pipes on Windows; `completions` generates shell completions.
- `src/run_terminal.rs` — `ptywright run` implementation for live stdin/stdout PTY bridging, raw-mode handling, and terminal-generated input filtering.
- `src/lib.rs` — public library surface; re-exports the layer types listed below.
- Generic layer modules (one module per abstraction, named after the concept):
  - `src/target.rs` — target configuration (executable, args, env, cwd, terminal size).
  - `src/session.rs` — PTY session lifecycle and exit status.
  - `src/screen.rs` — `vt100`-backed terminal parser, rich `ScreenSnapshot` with cell/style/cursor metadata.
  - `src/action.rs` — `Action`/`Key` input model.
  - `src/matcher.rs` — temporal matchers and wait policies.
  - `src/transcript.rs` — bounded in-memory transcript capture plus explicit raw file streaming opt-in.
  - `src/redaction.rs` — redaction helpers, caller-supplied additions, and the default RPC redaction policy.
  - `src/rpc.rs` — JSON-RPC server, shared `RpcServerState`, NDJSON + LSP framing, stdio and local IPC transports.
  - `src/paths.rs` — `~/.ptywright/` runtime directory resolution and per-layout accessors. `PTYWRIGHT_HOME` overrides the root.
  - `src/config.rs` — `~/.ptywright/config.toml` loader with forgiving defaults and forward-compatible parsing.
  - `src/logging.rs` — `tracing` init with daily rotation, configurable retention, redaction-aware writers, and mode-specific helpers (`init_for_run`, `init_for_serve_stdio`, `init_for_serve_socket`, `init_for_oneshot`). **Never write logs to stdout in `serve --stdio` mode** — stdout is reserved for JSON-RPC framing. The per-mode helpers enforce this for you; if you add a new entrypoint, pick one of them rather than calling `tracing_subscriber::fmt()` directly.
  - `src/error.rs` — crate-wide `Error` / `Result`.
- Plugin and adapter layer (application-specific code lives here, not in the generic layers):
  - `src/plugin.rs` — plugin manifests, permission declarations, runtime metadata enum, and built-in `claude_code_manifest`.
  - `src/lua_plugin.rs` — trusted embedded Lua 5.4 runtime (mlua, vendored) used by adapters.
  - `src/adapters/` — adapter implementations layered over the generic primitives; `claude_code.rs` is the current production adapter and drives Claude Code through Lua.
  - `plugins/claude-code/main.lua` — the trusted built-in Lua plugin that owns Claude-specific turn detection, stable-screen evidence, and usage-output parsing.
- Tests:
  - `tests/cli_tests.rs` — end-to-end checks for help/version output, basic PTY command execution, JSON-RPC stdio, and completions.
  - `tests/fixtures/claude_code/` — recorded screen fixtures for adapter transition tests; update these when Claude Code's UI shifts.
- Tooling and packaging:
  - `website/` — VitePress docs site.
  - `.github/workflows/` — CI, docs deploy (`pages.yml`), and release packaging (`release.yml`).
  - `flake.nix` — Nix package, app, formatter, and devshell.

When adding behavior, decide first which layer it belongs to. Claude-specific logic must not land in the generic modules; new generic primitives should not import from `adapters/`.

When exploring the repo, ignore build artifacts: `target/` (Cargo output) and `result` / `result-*` (Nix build symlinks). Both are gitignored and contain nothing worth grepping.

## Git workflow

Use normal branch-and-PR development moving forward.

- Start each task from an up-to-date `main` and create a focused branch.
- Branch names must use a Conventional Commit-style prefix and short kebab-case description:
  - `feat/<short-description>` for features.
  - `fix/<short-description>` for bug fixes.
  - `docs/<short-description>` for documentation-only changes.
  - `chore/<short-description>` for maintenance/tooling/workflow changes.
  - `test/<short-description>` for test-only changes.
  - `refactor/<short-description>` for behavior-preserving code restructuring.
- Do not use ad-hoc branch names like `wip`, `update`, `changes`, or personal initials.
- Do not push directly to `main` unless the user explicitly requests an emergency direct push.
- Keep commits small and use Conventional Commits: `<type>(optional-scope): <summary>`.
- Commit types should match the branch purpose where practical: `feat`, `fix`, `docs`, `chore`, `test`, `refactor`, `ci`, `perf`, or `build`.
- Push the branch and open a pull request with a concise summary and test results.
- Copilot review is requested automatically for non-draft PRs by the repository's Copilot review ruleset; the GitHub Rulesets settings are the source of truth. If it does not appear, request `copilot-pull-request-reviewer[bot]` manually.
- Let CI and Copilot review run on the PR and address failures or valid findings with additional commits on the same branch.
- Prefer merge commits or squash merges through GitHub; never force-push `main`.
- If a feature branch must be rebased after review starts, use `--force-with-lease` and mention it in the PR.
- Keep docs, README, `AGENTS.md`, and the `CLAUDE.md` symlink target synchronized in the same PR when behavior or workflow changes.

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
- Mark planned capabilities as planned; do not imply unimplemented Windows named-pipe serving, embedded plugin runtimes, WASM runtime, redaction policy, or high-fidelity Claude Code turn detection works today.
- Keep install and release docs honest about platform support.

## Conventions

- Use Conventional Commits.
- Prefer small, direct Rust modules with explicit types.
- Keep the CLI boring and composable.
- Prefer minimal diffs that fit the existing project style.
- Avoid god files: when a module starts mixing unrelated responsibilities or growing hard to review, refactor into focused modules as part of the same behavior change.
- Keep refactors behavior-preserving unless the PR explicitly changes behavior; preserve or improve test coverage while moving code.
- Run `ci-local` or the direct equivalent before handing off substantial changes.
