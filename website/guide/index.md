# What is ptywright?

ptywright is a Rust CLI and library project for driving interactive terminal applications through PTYs.

The goal is to make terminal automation feel local, inspectable, and deterministic while still interacting with real terminal programs. The core should be reusable across shells, REPLs, full-screen TUIs, and long-running processes.

## Current status

ptywright is a skeleton:

- `ptywright --help` prints the help menu.
- `ptywright --version` prints the package version.
- The library exposes package metadata and a small `Target` builder.
- Nix, CI, release, install, and docs plumbing are in place.
- Linux, macOS, and Windows are part of the intended support matrix.

::: tip Planned vs implemented
The docs describe the intended shape so the project can grow in the right direction. Pages call out planned capabilities explicitly when an API does not exist yet.
:::

## Design principles

- **General purpose first.** App-specific behavior belongs above the core PTY abstractions.
- **PTY-native.** Model terminal behavior as it appears to a real user.
- **Deterministic where possible.** Prefer explicit screen matching, turn boundaries, and captured transcripts.
- **Cross-platform by default.** Keep platform-specific APIs behind clear boundaries.
- **Small and local.** Avoid unnecessary daemons or cloud dependencies.
- **Library plus CLI.** Keep reusable Rust primitives under the crate and expose practical workflows through the binary.

## Planned layers

1. **Target** — what to spawn or attach to.
2. **Session** — PTY process lifecycle and terminal dimensions.
3. **Screen** — observed text, cursor state, alternate screen, and scrollback.
4. **Action** — keys, paste, resize, signals, and waits.
5. **Matcher** — prompts, status lines, completion markers, and error states.
6. **Turn** — request/response orchestration and transcript capture.
7. **Adapter** — application-specific workflows over the generic primitives.
