# ptywright — Agent Instructions

`AGENTS.md` is the canonical agent guide for this repository; `CLAUDE.md` is a symlink to it. Edits to either file land in `AGENTS.md` — keep that in mind when opening it as `CLAUDE.md`.

ptywright is a Rust CLI and library for driving interactive terminal applications through PTYs. The project is intentionally early, but the direction is a cross-platform automation toolkit with a generic core in Rust and application-specific plugins written in trusted Lua. The Rust layer never carries application-specific types or RPC namespaces.

## Product intent

Build a local, scriptable driver for terminal software:

- Spawn or attach to PTY-backed processes.
- Observe terminal state as a user would see it.
- Send deterministic input actions.
- Wait for prompts, screen states, turn boundaries, and failures.
- Layer shell, REPL, full-screen TUI, and app-specific plugins above reusable primitives.

The core must stay generic. Do not bake a single target application into core names, traits, modules, or docs. Application-specific behavior belongs in Lua plugins under `plugins/<name>/`.

Long-form design notes, milestone history, and open questions may live in a local-only, git-ignored `SPEC.md`. If that file exists in the working tree, read it before making structural changes; do not assume it is present in fresh clones.

## Abstraction rules

Prefer small, explicit layers:

1. **Target** — executable, arguments, environment, cwd, attach/spawn intent.
2. **Session** — PTY process lifecycle, dimensions, signals, exit status.
3. **Screen** — terminal parser output, cursor state, scrollback, alternate screen.
4. **Action** — key sequences, paste/write, resize, waits, interrupts.
5. **Matcher** — prompt detection, output predicates, timeout policies, error states.
6. **Turn** — request/response orchestration and transcript capture.
7. **Plugin** — app-specific workflows implemented in Lua over the generic layers. Plugins live under `plugins/<name>/` and are loaded through the `Extension` trait. There is no per-plugin Rust shim.

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
| check | `run-tests` | `cargo test --features _test-fixtures` |
| check | `ci-local` | fmt-check → check → clippy → test → build |
| check | `coverage` | `cargo llvm-cov --workspace --summary-only` |
| run | `ptywright` | `cargo run -- "$@"` |
| docs | `docs-dev` / `docs-build` / `docs-preview` | VitePress documentation |

Direct Cargo fallback (CI uses `--locked` for Cargo check/clippy/test jobs; mirror that when reproducing CI failures locally):

```bash
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked -- -D warnings
cargo test --locked --features _test-fixtures
cargo run -- --help
```

The `_test-fixtures` feature gates the test-only `ptywright-echo-tui` bin fixture. CI test runs (and `run-tests` / `ci-local` in the devshell) enable it; bare `cargo install ptywright` does not, which keeps the fixture out of users' `~/.cargo/bin`. Without the feature, `cargo test --locked` will fail to compile `tests/cli_tests.rs` because `env!("CARGO_BIN_EXE_ptywright-echo-tui")` won't be set — that's expected and intentional.

**`repl` feature** (interactive REPL client, `ptywright repl`) is on by default. To verify the lean build without `reedline` / `crossbeam-channel` / `nu-ansi-term`:

```bash
cargo check --locked --no-default-features
cargo clippy --locked --no-default-features -- -D warnings
cargo test --locked --no-default-features --features _test-fixtures
```

CI exercises both lanes: the default build (with `repl`) and the `--no-default-features` build, on Linux (check + clippy + test), macOS (check + test), and Windows (check only).

Run a single test:

```bash
cargo test --test cli_tests <test_name>   # one integration test in tests/cli_tests.rs
cargo test <module>::<test_name>          # one unit test inside a src/ module
cargo test <substring> -- --nocapture     # filter by substring and show stdout
cargo test --doc                          # rustdoc examples in public APIs
```

The default build embeds Lua 5.4 via `mlua` with the `vendored` feature, so source builds outside the Nix devshell need a working C compiler. The Rust toolchain is pinned in `rust-toolchain.toml`; rustup will fetch the pinned channel automatically when building outside Nix.

`clippy` is run without `--all-targets` to match CI exactly. If you want tests/examples linted too, run `cargo clippy --all-targets --locked -- -D warnings` locally, but do not change the devshell command or CI invocation without coordinating both.

When debugging RPC or adapter behavior, override the `tracing` filter at runtime with `PTYWRIGHT_LOG` (e.g. `PTYWRIGHT_LOG="info,ptywright::rpc=debug"`). `PTYWRIGHT_HOME` relocates the runtime directory (`~/.ptywright/` by default) for sandboxed tests or per-project isolation.

## Current architecture

The codebase is organized so each generic abstraction layer lives in one focused module, and plugins are layered on top without leaking back into the core.

- `src/main.rs` — clap CLI wiring. With no args it prints help; `--version` prints package version; `serve --stdio` exposes JSON-RPC with NDJSON or LSP-style framing; `serve --socket` exposes Unix sockets on macOS/Linux and named pipes on Windows; `serve --plugin <manifest.toml>` (repeatable) pre-loads trusted-local third-party plugins at startup; `serve --allow-plugin-load` enables the `plugin.load` / `plugin.unload` RPC methods for runtime registration; `completions` generates shell completions.
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
  - `src/extension.rs` — generic `Extension` trait, `ExtensionHandle` host loop, `ExtensionStateSnapshot`, body/status screen split (`STATUS_BAR_ROWS = 3`), and `LuaExtension` (the only implementor today). No application-specific identifiers live here; plugin-defined state names and intents flow through as strings.
  - `src/rpc.rs` — JSON-RPC server, shared `RpcServerState`, NDJSON + LSP framing, stdio and local IPC transports. Exposes the generic `adapter.*` surface plus `plugin.load` / `plugin.unload` (gated on `--allow-plugin-load`); there is no application-specific RPC namespace. Per-method permission gating against the bound plugin manifest is enforced at dispatch time (`-32004 PermissionDenied` with structured `data`). A sibling `adapter_plugin: HashMap<String, String>` mirrors adapter id → plugin name without per-adapter mutex protection so `plugin.unload`'s "live adapters" check cannot race with a long-running `adapter.wait` on a contended `Arc<Mutex<ExtensionEntry>>`.
  - `src/paths.rs` — `~/.ptywright/` runtime directory resolution and per-layout accessors. `PTYWRIGHT_HOME` overrides the root.
  - `src/config.rs` — `~/.ptywright/config.toml` loader with forgiving defaults and forward-compatible parsing.
  - `src/logging.rs` — `tracing` init with daily rotation, configurable retention, redaction-aware writers, and mode-specific helpers (`init_for_run`, `init_for_serve_stdio`, `init_for_serve_socket`, `init_for_oneshot`). **Never write logs to stdout in `serve --stdio` mode** — stdout is reserved for JSON-RPC framing. The per-mode helpers enforce this for you; if you add a new entrypoint, pick one of them rather than calling `tracing_subscriber::fmt()` directly.
  - `src/error.rs` — crate-wide `Error` / `Result`.
- Plugin layer (application-specific code lives entirely here, not in Rust):
  - `src/plugin.rs` — plugin manifests, permission declarations, runtime metadata enum, the `DefaultTarget` field plugins use to declare a default spawn program, and the `BUILTIN_PLUGINS` registry that pairs a manifest constructor with the embedded Lua source for every plugin shipped in the binary. Adding a new built-in plugin is a single `BuiltinPlugin { manifest, source }` entry in that slice. `PluginManifest::load_from_toml_path()` reads a third-party manifest + Lua source from disk; absolute paths, `..` traversal, and symlinks that resolve outside the manifest's own directory are all rejected. `PluginPermission::as_str()` returns the wire form used in error messages and JSON-RPC `data` payloads.
  - `src/lua_plugin.rs` — trusted embedded Lua 5.4 runtime (mlua, vendored) used by plugins. Installs the `ptywright.action.*` / `ptywright.matcher.*` host helpers gated on manifest-declared permissions.
  - `plugins/claude-code/main.lua` — the trusted built-in Lua plugin that owns Claude-specific turn detection, stable-screen evidence, workspace-trust dialog detection, and usage-output parsing. There is no Rust shim wrapping it.
- REPL client (gated `#[cfg(feature = "repl")]`, on by default — opt out with `cargo build --no-default-features`):
  - `src/repl/mod.rs` — public entry (`ReplArgs`, `Transport`, `Framing`, `run`) invoked from `Commands::Repl`. Builds the right transport, holds the stdio child guard for the REPL's lifetime, hands off to the reedline loop in `tui.rs`.
  - `src/repl/transport.rs` — framed JSON-RPC client (`RpcClient`) with NDJSON / LSP framing and a broadcast notification channel. Reader thread demuxes by `id` presence.
  - `src/repl/spawn.rs`, `src/repl/socket.rs` — transport bootstrappers for `--stdio -- <cmd>` and `--socket <path>` respectively. Mirror the server-side cfg split between Unix sockets and Windows named pipes.
  - `src/repl/command.rs` — the DSL lexer/parser/dispatcher (`session.spawn(...)`, `send.text("…")`, `wait(matches(r"…"))`, `:rpc <method> {json}`, `:focus`, `:tabs`, etc.). Intent names like `send_prompt` / `key` / `wait_turn_matcher` are string literals here only — they flow to plugins through the generic `adapter.send` / `adapter.wait` surface.
  - `src/repl/ctx.rs` — shared `ReplCtx` (tabs, focus) used by the dispatcher and the completer.
  - `src/repl/completer.rs`, `src/repl/highlighter.rs`, `src/repl/history.rs` — `reedline` trait impls (static DSL table + cached plugin names + live adapter ids for completion; nu-ansi-term-styled DSL highlighter; `FileBackedHistory` rooted at `Paths::repl_history_path()`).
  - `src/repl/tui.rs` — **sequential reedline-based REPL**. Each command renders as `pty> <syntax-highlighted DSL>` and the result follows on the next line as `↳ <dim summary>`. Line editing, completion, syntax highlighting, history, and ghost-text hinting are all delegated to reedline; this module owns the read-eval-print loop, the prompt, and how each `CmdOutcome` is printed (including the inline styled `ScreenSnapshot` rendering for `screen.snapshot()` / `view()`). Server-side notifications surface above the prompt via reedline's `ExternalPrinter`. **No application-specific identifiers live in any of these modules** — the REPL is a client of the generic `adapter.*` surface.
- Tests:
  - `tests/cli_tests.rs` — end-to-end checks for help/version output, basic PTY command execution, JSON-RPC stdio, and completions.
  - `tests/lua_classifier_tests.rs` — auto-enrolling classifier regression matrix. Loads every `<name>.txt` fixture under `tests/fixtures/claude_code/` with a sibling `<name>.expected.json` and drives it through `LuaExtension::built_in("claude-code")`.
  - `tests/lua_plugin_intents.rs` — per-intent contract tests (`send_prompt`, `approve`, `deny`, `cancel`, `approve_trust`, `deny_trust`, `dismiss_welcome`, `wait_turn_matcher`, the `cancelling` hold-state) driven through the generic `ExtensionHandle` API. Doubles as a reference for plugin authors writing new TUI plugins.
  - `tests/repl_tests.rs` — gated `#[cfg(feature = "repl")]`. Drives `RpcClient` + `command::dispatch` against an in-process `serve_ndjson_with_state` over pipes: capabilities, full spawn→state→close cycle, raw `:rpc` passthrough, adapter-flavored `session.changed` notifications.
  - `tests/fixtures/claude_code/` — recorded screen fixtures for the classifier; update these when Claude Code's UI shifts. Adding a new fixture is a single-PR documentation-only change: drop a `<name>.txt` and sibling `<name>.expected.json` and the matrix picks them up.
- Tooling and packaging:
  - `website/` — VitePress docs site.
  - `.github/workflows/` — CI, docs deploy (`pages.yml`), and release packaging (`release.yml`).
  - `flake.nix` — Nix package, app, formatter, and devshell.
  - `install.sh` — curl-able installer for prebuilt release artifacts (referenced from the docs site).
  - `config.example.toml` — annotated reference for `~/.ptywright/config.toml`; keep in sync with `src/config.rs` when adding tunables.
  - `CHANGELOG.md` — hand-maintained, Keep-a-Changelog style. Add user-visible changes to the `[Unreleased]` section in the same PR; release tooling promotes it on tag.

When adding behavior, decide first which layer it belongs to. **No Claude-specific or other application-specific identifiers (state names, intent names, fixture conventions) belong anywhere in `src/` outside the corresponding entry in `src/plugin.rs::BUILTIN_PLUGINS`.** Application-specific code lives in `plugins/<name>/main.lua`. Adding a new built-in TUI plugin is:

1. Write `plugins/<name>/main.lua` exporting `classify`, the intent functions you want callers to be able to invoke through `adapter.send` (and `wait_*_matcher` functions for `adapter.wait`).
2. Add a manifest constructor next to `claude_code_manifest()` in `src/plugin.rs`.
3. Add a single `BuiltinPlugin { manifest: foo_manifest, source: include_str!("../plugins/foo/main.lua") }` entry to `BUILTIN_PLUGINS` in the same file. The manifest and the embedded source travel together — `LuaExtension::built_in(name)` does the lookup directly against that slice, so there is no second registration to keep in sync.

That's the entire integration surface — no per-plugin Rust types, no per-plugin RPC namespace, no per-plugin matcher kinds. Callers reach the new plugin through the generic `adapter.*` JSON-RPC surface or `LuaExtension::built_in("<name>")` from Rust.

**Headless terminal geometry.** TUI classifiers usually depend on line wrapping — the host's last-resort `40x120` is *not* a safe default for any non-trivial plugin. Declare a classifier-stable preset in your `DefaultTarget` (`rows` / `cols`) so `adapter.start` picks it up when callers omit geometry. The built-in `claude-code` plugin ships **200×60**: wide enough that the status bar, `❯` / `>` prompt glyphs, "Total cost: …" usage screen, and the longest tool-use status lines never wrap. New TUI plugins should pick a similar preset that comfortably fits the longest unwrapped line their classifier reads — `cols >= 160`, `rows >= 40` is a reasonable starting point; tighten only after you have classifier fixtures that prove the smaller size is stable.

**For trusted-local third-party plugins** that should not ship in-tree, no Rust changes are needed at all. Author `manifest.toml` + `main.lua` (declaring `permissions` and an optional `[default_target]`) and load through one of:

- CLI: `ptywright serve --plugin <path/to/manifest.toml>` (repeatable, registered at startup).
- JSON-RPC: `plugin.load { manifest_path }` after starting the server with `--allow-plugin-load` (gated by the new `RpcSharedState.allow_plugin_load` flag — without it both `plugin.load` and `plugin.unload` return `-32004 PermissionDenied` with `data.reason = "server_did_not_grant_plugin_load"`).
- Rust: `RpcServerState::register_plugin(manifest, source)`.

Per-method permission gating in `src/rpc.rs` consults the bound manifest's declared `permissions` on every `adapter.*` call (`adapter.start` → `session.spawn`, `adapter.send` → `input.write`, `adapter.wait` → `matcher.wait`, `adapter.snapshot` / `adapter.state` / `adapter.inspect` → `screen.read`, `adapter.transcript` → `transcript.read`, `adapter.close` → `session.kill`; `adapter.list` / `adapter.live` are read-only registry queries with no gate). Denied calls return `-32004 PermissionDenied` with structured `data: { method, required_permission }`. Third-party plugins should declare the minimal permission subset they need; the built-in claude-code declares all seven.

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
- For user-visible changes (CLI flags, RPC methods, config keys, runtime layout), add an entry under the `[Unreleased]` section of `CHANGELOG.md` in the same PR. Release tooling promotes `[Unreleased]` on tag — leaving an entry out means it won't appear in release notes.

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
