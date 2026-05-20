# ptywright

[![CI](https://github.com/utensils/ptywright/actions/workflows/ci.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/ci.yml)
[![Deploy Docs](https://github.com/utensils/ptywright/actions/workflows/pages.yml/badge.svg)](https://github.com/utensils/ptywright/actions/workflows/pages.yml)
[![codecov](https://codecov.io/gh/utensils/ptywright/graph/badge.svg)](https://codecov.io/gh/utensils/ptywright)

**A cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.**

ptywright is an early general-purpose PTY/TUI automation toolkit. It is designed to drive interactive terminal applications from code without coupling the core abstractions to any one program. A generic `Extension` trait sits above the PTY/session/screen/action/matcher primitives, with Claude Code shipped as the first plugin under that trait.

The library now includes target, session, rich screen snapshot, action, temporal and plugin-defined matchers, bounded transcripts with turn segmentation marks and optional raw file streaming, redaction, JSON-RPC with atomic `adapter.turn` / cooperative `adapter.cancel_wait` / introspection via `plugin.describe`, the generic `Extension` layer with in-process `Session::events` / `ExtensionHandle::subscribe`, a Lua-backed interactive Claude Code adapter with stable-screen turn evidence and structured `metadata.*` channels (permission, plan, error, usage, status, dialog correlation), plugin manifest/runtime primitives with per-method permission gating, a `Session` SIGTERM→SIGKILL signal ladder, and shell completion primitives backed by real PTYs. The CLI includes `run` for live stdin/stdout PTY debugging, `serve --stdio` for NDJSON or LSP-style JSON-RPC automation, multi-client local IPC via Unix sockets on macOS/Linux and named pipes on Windows, `serve --plugin <manifest.toml>` for trusted-local third-party plugins, `logs --tail` for following the rotated log file, `repl` for an interactive embedded-Lua client over a running server (auto-spawning one when none is listening), and `completions` for shell setup.

Docs: <https://utensils.io/ptywright/>

## Quickstart

```bash
ptywright --help
ptywright --version
ptywright run -- /bin/sh -lc 'printf ready'
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | ptywright serve --stdio
printf '{"jsonrpc":"2.0","id":1,"method":"adapter.list"}\n' | ptywright serve --stdio | jq '.result.plugins[].name'
printf '{"jsonrpc":"2.0","id":1,"method":"adapter.start","params":{"plugin":"claude-code","program":"/bin/sh","args":["-lc","cat"]}}\n' | ptywright serve --stdio | jq '.result | {adapter, plugin}'
ptywright serve --stdio --framing lsp
ptywright serve --socket /tmp/ptywright.sock
source <(ptywright completions zsh)
```

## Install from source

The default build embeds Lua 5.4 for trusted adapter plugins, so source builds need a working C compiler in addition to Rust. The Nix dev shell provides the expected toolchain on macOS/Linux.

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
6. Bounded transcript capture with explicit raw transcript file streaming opt-in.
7. JSON-RPC over stdio or multi-client local IPC for external automation clients, with NDJSON and LSP-style framing.
8. Generic `Extension` trait with `ExtensionHandle` host loop; Claude Code ships as the first plugin under that trait, driven through the `adapter.*` JSON-RPC surface.
9. Plugin manifests, permission declarations, and trusted embedded Lua runtime for adapter orchestration. Trusted-local third-party plugins load via the `ptywright serve --plugin <manifest.toml>` CLI flag or the `plugin.load` JSON-RPC method (gated by `--allow-plugin-load`).
10. Per-method permission gating at the JSON-RPC dispatcher — every `adapter.*` method consults the bound plugin manifest's declared permissions and rejects calls with `-32004 PermissionDenied` carrying structured `data`.
11. Per-user runtime directory under `~/.ptywright/` with structured logging (daily rotation, redaction-aware writers).
12. Interactive REPL client (`ptywright repl`, default-on `repl` Cargo feature): embedded Lua 5.4 evaluator over the generic `adapter.*` surface, reedline line-editing with Lua-introspection completion / highlighting / multi-line continuation, auto-spawn of a background server when none is listening, tmux-style attach, and inline notification rendering.
13. Shell completion generation for bash, zsh, fish, elvish, and PowerShell.
14. Rich screen snapshots with cell/style/mode metadata.
15. Redaction helpers with built-in and caller-supplied patterns plus default RPC redaction for sensitive-looking output.
16. In-process event subscription on `Session` and `ExtensionHandle`; structured matcher outcomes on `adapter.wait`; cooperative cancellation via `CancellationToken` / `adapter.cancel_wait`; plugin-defined `Matcher::Lua` predicates evaluated against a bound `PluginRegistry`.
17. Atomic `adapter.turn` (send + wait under the per-adapter mutex), `adapter.resume` chaining across PTY restarts, and `plugin.describe` runtime introspection of intents, wait matchers, and classifier states.
18. Cross-platform PID + signal ladder (`Session::pid`, `Session::signal`, `Session::terminate` with SIGTERM→SIGKILL escalation) and plugin-mandated env (`default_target.required_env`) plus `Target::clear_env` for sandboxed spawns.
19. Bounded transcript turn segmentation (`Transcript::mark` / `marker` / `slice_between`), classifier-driven `host_marks`, and the `Action::MarkTranscript` plan primitive.

## Planned layers

1. More Claude Code real-world fixtures and transition tests as upstream Claude Code UI changes.
2. A second built-in adapter (Codex / shell) to validate the plugin model against a non-Claude TUI.
3. Optional WASM only if untrusted marketplace-style plugins become a concrete priority.

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
cargo test --locked --features _test-fixtures
cargo run -- --help
```

## Runtime directory and logging

ptywright keeps configuration and rotated log files under `~/.ptywright/` (override with `PTYWRIGHT_HOME=/some/path`). Logs are written through `tracing` with daily rotation, 14-day retention by default, and built-in redaction of secret-shaped values. Override the filter at runtime with `PTYWRIGHT_LOG`:

```bash
PTYWRIGHT_LOG="info,ptywright::rpc=debug" ptywright serve --stdio
```

See [`config.example.toml`](config.example.toml) for the full set of tunables and the [Runtime directory](https://utensils.io/ptywright/guide/runtime-directory.html) docs for layout details. `ptywright run` writes only to file (it owns your terminal); `serve --stdio` and `serve --socket` write to file and stderr while keeping stdout reserved for JSON-RPC framing.

See [AGENTS.md](AGENTS.md) for repository guidance.

## License

MIT — see [LICENSE](LICENSE).
