# Platforms

ptywright is intended to work across macOS, Linux, and Windows.

## Current status

The current skeleton is pure Rust plus `clap`, so it builds on the default Rust targets used by CI. Real PTY support is planned and will need platform-aware implementation work.

| Platform | Status         | Notes                                                                                                 |
| -------- | -------------- | ----------------------------------------------------------------------------------------------------- |
| macOS    | Planned target | Nix devshell and release artifacts cover Apple Silicon and Intel.                                     |
| Linux    | Planned target | Nix devshell and release artifacts cover x86_64 and aarch64.                                          |
| Windows  | Planned target | CI and release packaging include x86_64 MSVC. PTY implementation will need Windows-specific handling. |

## Development environments

- Use Nix on macOS and Linux when available.
- Use Cargo directly on Windows.
- Keep code paths portable unless a module is explicitly platform-specific.

## Implementation notes for future PTY work

- Unix PTYs and Windows ConPTY should sit behind a session abstraction.
- Path handling should use `Path`/`PathBuf`, not string concatenation.
- Tests should avoid shell-specific assumptions unless they are platform-gated.
- Release artifacts should keep platform naming explicit.
