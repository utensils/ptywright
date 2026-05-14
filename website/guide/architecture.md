# Architecture

ptywright is an early PTY/TUI automation runtime. The core is intentionally generic: application-specific behavior, including the interactive Claude Code adapter, sits above reusable terminal primitives.

## Repository layout

```text
ptywright/
├── src/
│   ├── action.rs      # serializable input/lifecycle actions
│   ├── adapters/      # app-specific adapters such as Claude Code
│   ├── error.rs       # public error/result types
│   ├── lib.rs         # public library surface
│   ├── main.rs        # clap CLI entrypoint
│   ├── matcher.rs     # screen/transcript predicates
│   ├── rpc.rs         # JSON-RPC server and framing helpers
│   ├── screen.rs      # terminal engine seam, parser, and snapshots
│   ├── session.rs     # PTY-backed process lifecycle
│   ├── target.rs      # spawn configuration
│   └── transcript.rs  # bounded output transcript
├── tests/             # CLI integration tests
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

| Layer      | Responsibility                                             | Should avoid                  |
| ---------- | ---------------------------------------------------------- | ----------------------------- |
| Target     | Program, args, cwd, environment, terminal size             | PTY lifecycle details         |
| Session    | Child process, PTY handle, reader/writer, resize, exit     | App prompt semantics          |
| Screen     | Parsed terminal view, cursor, scrollback, alternate screen | Input timing policy           |
| Transcript | Bounded retained PTY output text                           | Terminal rendering decisions  |
| Action     | Keys, writes, paste, resize, interrupt, EOF, kill          | App-specific success rules    |
| Matcher    | Screen/transcript predicates and timeout evidence          | Owning the process            |
| RPC        | Protocol framing, session registry, method dispatch        | Human output on stdout        |
| Turn       | Send input, wait for completion, capture transcript        | Hard-coded app names          |
| Adapter    | App-specific workflows                                     | Reimplementing PTY primitives |

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
- Plugin manifest and permission types for future trusted extensions.
- Dynamic shell completion generation through `clap_complete`.

The public API hides backend crate types so ptywright can evolve the PTY or terminal parser later. Screen snapshots expose portable cell/style/mode metadata instead of `vt100` types.

## Interactive Claude Code adapter

ptywright targets interactive Claude Code through the terminal TUI. It does not optimize around `claude -p` or non-interactive Agent SDK flows.

The Claude Code adapter uses only generic primitives:

- spawn `claude` in a PTY-backed `Session`;
- observe `ScreenSnapshot` and transcript evidence;
- send `Action` values for prompts, approval keys, interrupts, and resize;
- wait with `Matcher` predicates;
- expose Claude-specific state only in adapter APIs and `claude.*` RPC methods.

This keeps the core useful for shells, REPLs, full-screen TUIs, and other long-running terminal programs.

## Testing direction

Behavior changes should follow test-driven development where practical. Add or update tests around public primitives before implementation, keep PTY fixtures deterministic, and document platform-specific limitations when a test cannot be portable.
