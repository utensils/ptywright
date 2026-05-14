# Architecture

ptywright is intentionally minimal today. The repository is structured so PTY/TUI automation can grow without reworking package, release, and documentation infrastructure.

## Repository layout

```text
ptywright/
├── src/
│   ├── lib.rs      # public library surface
│   └── main.rs     # clap CLI entrypoint
├── tests/          # CLI integration tests
├── website/        # VitePress docs
├── flake.nix       # Nix package, app, devshell, formatter
├── install.sh      # GitHub Release installer for macOS/Linux
└── .github/        # CI, docs deploy, release packaging
```

## Intended runtime model

The future driver should separate generic terminal automation from app-specific behavior:

```text
caller / CLI / integration
  │
  ▼
turn orchestration
  │
  ├─ target configuration
  ├─ PTY session lifecycle
  ├─ terminal screen parser / observer
  ├─ input actions: keys, paste, resize, signals
  ├─ wait and matcher primitives
  └─ adapters
       ├─ shell / REPL
       ├─ full-screen TUI
       └─ long-running interactive process
```

## Abstraction boundaries

| Layer   | Responsibility                                             | Should avoid                  |
| ------- | ---------------------------------------------------------- | ----------------------------- |
| Target  | Program, args, cwd, environment, spawn/attach intent       | PTY lifecycle details         |
| Session | Child process, PTY handle, size, signals, exit             | App prompt semantics          |
| Screen  | Parsed terminal view, cursor, scrollback, alternate screen | Input timing policy           |
| Action  | Keys, writes, paste, resize, interrupt, wait               | App-specific success rules    |
| Matcher | Prompt/output predicates, timeout policy, error detection  | Owning the process            |
| Turn    | Send input, wait for completion, capture transcript        | Hard-coded app names          |
| Adapter | Shell/REPL/TUI-specific workflows                          | Reimplementing PTY primitives |

## Adapter direction

An application adapter can eventually:

- spawn or attach to an interactive terminal process;
- send prompts, commands, keystrokes, or pasted input;
- observe screen state and optional side-channel logs/transcripts;
- detect turn completion, prompt readiness, and error states;
- expose a deterministic library API while still driving a real TUI.

Adapters should live above the generic PTY primitives so ptywright remains useful across many terminal applications.
