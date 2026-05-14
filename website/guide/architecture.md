# Architecture

ptywright is an early PTY/TUI automation runtime. The core is intentionally generic: application-specific behavior, including the future interactive Claude Code adapter, sits above reusable terminal primitives.

## Repository layout

```text
ptywright/
├── src/
│   ├── action.rs      # serializable input/lifecycle actions
│   ├── error.rs       # public error/result types
│   ├── lib.rs         # public library surface
│   ├── main.rs        # clap CLI entrypoint
│   ├── matcher.rs     # screen/transcript predicates
│   ├── screen.rs      # terminal parser and snapshots
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
caller / CLI / future JSON-RPC client
  │
  ▼
Target
  │ program, args, cwd, env, terminal size
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
  │ future shell, REPL, full-screen TUI, and Claude Code workflows
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
| Turn       | Send input, wait for completion, capture transcript        | Hard-coded app names          |
| Adapter    | App-specific workflows                                     | Reimplementing PTY primitives |

## Current implementation

The first implementation uses:

- `portable-pty` for cross-platform PTY creation and child process management.
- `vt100` for the initial terminal parser and rendered screen snapshots.
- A reader thread per session that processes output in batches.
- A monotonic sequence number for screen/transcript changes.
- A bounded in-memory transcript to avoid unbounded output growth.
- Event-driven matcher waits using a condition variable, not sleep polling.

The public API hides backend crate types so ptywright can evolve the PTY or terminal parser later.

## Interactive Claude Code direction

ptywright will target interactive Claude Code through the terminal TUI. It will not optimize around `claude -p` or non-interactive Agent SDK flows.

The future Claude Code adapter should use only generic primitives:

- spawn `claude` in a PTY-backed `Session`;
- observe `ScreenSnapshot` and transcript evidence;
- send `Action` values for prompts, approval keys, interrupts, and resize;
- wait with `Matcher` predicates;
- expose Claude-specific state only in adapter APIs.

This keeps the core useful for shells, REPLs, full-screen TUIs, and other long-running terminal programs.

## Testing direction

Behavior changes should follow test-driven development where practical. Add or update tests around public primitives before implementation, keep PTY fixtures deterministic, and document platform-specific limitations when a test cannot be portable.
