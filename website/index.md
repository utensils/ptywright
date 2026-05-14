---
layout: home

hero:
  name: ptywright
  text: Drive terminal apps from Rust
  tagline:
    A cross-platform Rust CLI and library for automating interactive terminal
    programs through PTYs. Built around reusable session, screen, action, matcher,
    and adapter layers instead of one target application.
  image:
    src: /favicon.svg
    alt: ptywright terminal mark
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: Architecture
      link: /guide/architecture
    - theme: alt
      text: GitHub
      link: https://github.com/utensils/ptywright

features:
  - title: PTY-first core
    details:
      Model terminal behavior through spawn, observe, write, resize, wait, and
      transcript primitives that match how a real terminal behaves.
  - title: Adapter-ready
    details:
      Shells, REPLs, full-screen TUIs, and app-specific workflows belong above
      the generic PTY/session/screen/action layers.
  - title: Cross-platform target
    details:
      The project is shaped for macOS, Linux, and Windows, with Nix as a Unix
      devshell and Cargo/GitHub Actions for portable Rust checks.
  - title: Deterministic orchestration
    details:
      Future turn APIs should use explicit matchers, timeout policies, screen
      snapshots, and transcripts instead of fragile sleeps.
  - title: Library plus CLI
    details:
      Reusable Rust abstractions live in the crate while the binary stays boring,
      composable, and script-friendly.
  - title: Ready plumbing
    details:
      Cargo metadata, Nix flake, CI, release packaging, docs deploy, install
      script, and tests are in place so implementation work can start cleanly.
---

## Status

ptywright is intentionally a fresh skeleton today.

<div class="terminal">
<span class="prompt">$</span> ptywright --help<br>
<span class="prompt">$</span> ptywright --version
</div>

The first release establishes the package, binary, documentation site, cross-platform CI direction, and release plumbing. PTY process control and terminal observation APIs are planned next.

## Intended library shape

```rust
use ptywright::Target;

let target = Target::new("python").arg("-i");
```

Future versions will expand this into session lifecycle, screen observation, input actions, matchers, turn orchestration, and app-specific adapters.

## Local development

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
nix develop
ci-local
```

Start with the [Quickstart](/guide/quickstart) or review the [architecture](/guide/architecture).
