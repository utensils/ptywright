---
layout: home

hero:
  name: ptywright
  text: Drive terminal apps from Rust
  tagline:
    A Rust CLI and library for automating interactive TUI programs through
    PTYs. Built around general-purpose terminal automation primitives instead
    of one target application.
  actions:
    - theme: brand
      text: Get Started →
      link: /guide/
    - theme: alt
      text: CLI Reference
      link: /reference/cli
    - theme: alt
      text: GitHub
      link: https://github.com/utensils/ptywright

features:
  - title: PTY-first
    details:
      The core abstraction is a terminal session, not an API provider. Spawn,
      observe, write, and eventually replay interactive programs as they appear
      in a real terminal.
  - title: App-agnostic core
    details:
      Shells, REPLs, full-screen TUIs, and long-running processes should all fit
      through the same lower-level primitives.
  - title: Adapter-ready
    details:
      Application-specific behavior belongs in adapters layered above reusable
      PTY session, screen, input, wait, and matcher types.
  - title: Scriptable CLI
    details:
      The binary starts with help and version only. Future commands should stay
      predictable, machine-readable, and easy to compose.
  - title: Rust library surface
    details: The crate is set up for reusable target, session, action, screen
      observation, matcher, and turn orchestration primitives.
  - title: Batteries included plumbing
    details:
      Nix flake, devshell, CI, docs, release packaging, install script, and tests
      are already wired up so implementation work can start cleanly.
---

## Today

```bash
$ ptywright --help
$ ptywright --version
```

The current release is intentionally a skeleton. It establishes the crate,
binary, documentation site, and release plumbing.

## Intended shape

```rust
use ptywright::Target;

let target = Target::new("python").arg("-i");
```

Future versions will add PTY process management, terminal observation, input
actions, and adapter-level turn orchestration.

## Install from a local checkout

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
nix develop
cargo run -- --help
```

See [Quickstart](/guide/quickstart) for the current development workflow.
