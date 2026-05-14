# What is ptywright?

ptywright is a Rust CLI and library project for driving interactive terminal applications through PTYs.

The goal is to make it possible to automate terminal programs while keeping the implementation local, inspectable, and adapter-driven. The core should stay independent of any single application so higher-level adapters can target shells, REPLs, full-screen TUIs, and other long-running processes.

## Current status

ptywright is a skeleton:

- `ptywright --help` prints the help menu.
- `ptywright --version` prints the package version.
- The library exposes minimal package metadata and a placeholder `Target` type.
- Nix, CI, release, install, and docs plumbing are in place.

## Design principles

- **General purpose first.** App-specific behavior belongs above the core PTY abstractions.
- **PTY-native.** Model terminal behavior as it appears to a real user.
- **Deterministic where possible.** Prefer explicit screen matching, turn boundaries, and captured transcripts.
- **Small and local.** Avoid unnecessary daemons or cloud dependencies.
- **Library plus CLI.** Keep reusable Rust primitives under the crate and expose practical workflows through the binary.

## Planned layers

1. **Target** — what to spawn or attach to.
2. **Session** — PTY process lifecycle and terminal dimensions.
3. **Screen** — observed text, cursor state, alternate screen, and scrollback.
4. **Action** — keys, paste, resize, signals, and waits.
5. **Matcher** — prompts, status lines, completion markers, and error states.
6. **Adapter** — application-specific orchestration over the generic primitives.
