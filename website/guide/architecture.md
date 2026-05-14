# Architecture

ptywright is intentionally minimal today. The project is structured so the PTY/TUI automation implementation can grow without reworking package, release, and documentation infrastructure.

## Repository layout

```text
ptywright/
├── src/
│   ├── lib.rs      # public library surface
│   └── main.rs     # clap CLI entrypoint
├── tests/          # CLI integration tests
├── website/        # VitePress docs
├── flake.nix       # Nix package, app, devshell, formatter
├── install.sh      # GitHub Release installer
└── .github/        # CI, docs deploy, release packaging
```

## Intended runtime model

The future driver should separate generic terminal automation from app-specific behavior:

```text
caller / CLI
  │
  ▼
ptywright orchestration
  │
  ├─ PTY session lifecycle
  ├─ terminal screen parser / observer
  ├─ input actions: keys, paste, resize, signals
  ├─ wait/matcher primitives
  └─ adapters
       ├─ shell / REPL
       ├─ full-screen TUI
       └─ long-running interactive process
```

## Adapter direction

An application adapter can eventually:

- spawn or attach to an interactive terminal process;
- send prompts, commands, keystrokes, or pasted input;
- observe screen state and optional side-channel logs/transcripts;
- detect turn completion, prompt readiness, and error states;
- expose a library API that feels deterministic while still driving a real TUI.

Adapters should live above the generic PTY primitives so ptywright remains useful across many terminal applications.
