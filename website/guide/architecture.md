# Architecture

ptywright is an early PTY/TUI automation runtime. The core is intentionally generic: application-specific behavior, including the interactive Claude Code adapter, sits above reusable terminal primitives. The generic `Extension` trait is the seam between those primitives and any specific TUI's classifier and intents.

## Repository layout

```text
ptywright/
├── src/
│   ├── action.rs      # serializable input/lifecycle actions
│   ├── adapters/      # app-specific adapter shims (currently claude_code.rs)
│   ├── config.rs      # ~/.ptywright/config.toml loader
│   ├── error.rs       # public error/result types
│   ├── extension.rs   # Extension trait, ExtensionHandle, ExtensionStateSnapshot
│   ├── lib.rs         # public library surface
│   ├── logging.rs     # tracing init with rotation, retention, and redaction
│   ├── main.rs        # clap CLI entrypoint
│   ├── lua_plugin.rs  # trusted Lua plugin runtime for adapter orchestration
│   ├── matcher.rs     # screen/transcript predicates
│   ├── paths.rs       # ~/.ptywright runtime directory resolution
│   ├── plugin.rs      # plugin manifest, permissions, host capabilities
│   ├── rpc.rs         # JSON-RPC server and framing helpers
│   ├── screen.rs      # terminal engine seam, parser, and snapshots
│   ├── session.rs     # PTY-backed process lifecycle
│   ├── target.rs      # spawn configuration
│   └── transcript.rs  # bounded output transcript
├── plugins/           # built-in trusted Lua plugins embedded in the binary
├── tests/             # CLI integration tests and recorded fixtures
├── website/           # VitePress docs
├── flake.nix          # Nix package, app, devshell, formatter
├── install.sh         # GitHub Release installer for macOS/Linux
└── .github/           # CI, docs deploy, release packaging
```

## Runtime model

```text
caller / CLI / JSON-RPC client
  │
  ▼
Target
  │ program, args, cwd, env, terminal size
  ▼
JSON-RPC server (optional stdio automation boundary)
  │ protocol parsing, session registry, method dispatch
  ▼
Session
  │ PTY process lifecycle, reader, writer, resize, kill, wait
  ▼
Terminal + Transcript
  │ rendered screen snapshots and bounded raw-ish text history
  ▼
Action + Matcher
  │ deterministic input and event-driven waits
  ▼
Turn orchestration / adapters
  │ shell, REPL, full-screen TUI, and Claude Code workflows
```

## Abstraction boundaries

| Layer      | Responsibility                                             | Should avoid                        |
| ---------- | ---------------------------------------------------------- | ----------------------------------- |
| Target     | Program, args, cwd, environment, terminal size             | PTY lifecycle details               |
| Session    | Child process, PTY handle, reader/writer, resize, exit     | App prompt semantics                |
| Screen     | Parsed terminal view, cursor, scrollback, alternate screen | Input timing policy                 |
| Transcript | Bounded retained PTY output text                           | Terminal rendering decisions        |
| Action     | Keys, writes, paste, resize, interrupt, EOF, kill          | App-specific success rules          |
| Matcher    | Screen/transcript predicates and timeout evidence          | Owning the process                  |
| RPC        | Protocol framing, session registry, method dispatch        | Human output on stdout              |
| Extension  | Plugin-backed classifier + intent plans over a session     | PTY IO, parser changes, RPC framing |
| Turn       | Send input, wait for completion, capture transcript        | Hard-coded app names                |
| Adapter    | App-specific façade over an `ExtensionHandle`              | Reimplementing PTY primitives       |

The `Extension` layer lives in `src/extension.rs` and is the boundary between the generic core and any specific TUI. It is intentionally application-agnostic: the trait classifies plugin-defined state strings and builds generic [`Action`] / [`Matcher`] plans, with no Claude-specific identifiers in the trait surface.

- [`Extension`](https://github.com/utensils/ptywright/blob/main/src/extension.rs) — three-method trait: `classify(ctx)`, `plan(intent, params)`, `wait_matcher(intent, params)`. Plugin runtimes implement it; the generic core consumes it.
- [`LuaExtension`] — the only implementor shipped today. Wraps a trusted embedded `LuaPlugin` and exposes it through the trait. A future WASM or external-process runtime would slot in here without changing the rest of the core.
- [`ExtensionHandle`] — owns a [`Session`] plus a boxed [`Extension`]. Drives the host loop (classify, apply intent action plans, wait on intent matchers) and forwards `last_intent` plus a stability threshold into each classify call.
- [`ExtensionStateSnapshot`] — `{state, confidence, evidence, sequence, candidates}` where `state` is a plugin-defined string. Adapter shims translate it into their own typed enum (e.g. `ClaudeCodeStateSnapshot`).

Classifier context split by region: `ExtensionHandle` splits the rendered screen into `body_text` (everything above the status bar) and `status_text` (the bottom `STATUS_BAR_ROWS = 3` lines) before handing it to `Extension::classify`. Body classifiers run against `body_text` so a benign status string like `⏵⏵ bypass permissions on` cannot false-positive on substring matches in the body. Short screens (heights at or below `STATUS_BAR_ROWS * 2`) are returned as body-only so small fixtures and small windows don't lose all their content to the status bucket. The same split is reused by `claude.inspect` / `adapter.inspect` for diagnostic dumps.

## Current implementation

The first implementation uses:

- `portable-pty` for cross-platform PTY creation and child process management.
- `vt100` behind an internal terminal engine seam for the initial parser and rendered screen snapshots.
- A reader thread per session that processes output in batches.
- A monotonic sequence number for screen/transcript changes.
- A bounded in-memory transcript to avoid unbounded output growth.
- Event-driven matcher waits using a condition variable, not sleep polling.
- Temporal/lifecycle matchers for stable screens and process-exit observation.
- JSON-RPC 2.0 over stdio with NDJSON and LSP-style `Content-Length` framing for external automation clients.
- Local Unix socket serving on macOS/Linux for longer-lived local automation processes.
- Opt-in coalesced JSON-RPC notifications for session changes and exits.
- Plugin manifest, permission, and runtime types for trusted extensions.
- Embedded Lua for built-in adapter orchestration, currently used by the Claude Code adapter.
- Dynamic shell completion generation through `clap_complete`.
- A per-user runtime directory at `~/.ptywright/` for config and rotated log files. See [Runtime directory](./runtime-directory.md).
- Structured logging through `tracing` with daily-rotated files, configurable retention, and per-record redaction so ptywright-owned diagnostics never leak secrets to disk or stderr.

The public API hides backend crate types so ptywright can evolve the PTY or terminal parser later. Screen snapshots expose portable cell/style/mode metadata instead of `vt100` types.

## Interactive Claude Code adapter

ptywright targets interactive Claude Code through the terminal TUI. It does not optimize around `claude -p` or non-interactive Agent SDK flows.

The Claude Code adapter at `src/adapters/claude_code.rs` is a thin typed façade over `ExtensionHandle`:

- Rust spawns `claude` in a PTY-backed `Session`;
- Rust constructs an `ExtensionHandle` with `LuaExtension::built_in("claude-code")`;
- the built-in Lua plugin at `plugins/claude-code/main.lua` classifies Claude Code screen states;
- the Lua plugin returns generic `Action` values for prompts, approvals, denials, interrupts, and the trust-dialog numeric selections;
- the Lua plugin returns generic `Matcher` values for turn waits;
- the adapter shim translates the plugin's state strings into the typed `ClaudeCodeState` enum.

The Rust core has no Claude-specific identifiers outside `src/adapters/`. Generic modules under `src/` do not import from `src/adapters/`, and the `Extension` trait surface is plugin-name-agnostic. New TUIs land as additional Lua plugins (with an optional typed adapter shim alongside `claude_code.rs`) and reuse the same `ExtensionHandle` host loop.

## Testing direction

Behavior changes should follow test-driven development where practical. Add or update tests around public primitives before implementation, keep PTY fixtures deterministic, and document platform-specific limitations when a test cannot be portable.
