# Platforms

ptywright is intended to work across macOS, Linux, and Windows.

## Current status

The initial PTY implementation uses `portable-pty`, which maps to Unix PTYs on macOS/Linux and ConPTY on Windows. The crate is built and checked on all three operating systems in CI.

| Platform | Status            | Notes                                                                  |
| -------- | ----------------- | ---------------------------------------------------------------------- |
| macOS    | Initial PTY path  | CI runs check, clippy, tests, and release build.                       |
| Linux    | Initial PTY path  | CI runs check, clippy, tests, and release build.                       |
| Windows  | Compile validated | CI builds/checks the PTY code. Deterministic ConPTY fixtures are next. |

## Current limitations

- Unix PTY behavior has deterministic tests for simple command output.
- Windows ConPTY support is compiled in, but command-output fixtures are temporarily gated while deterministic Windows PTY test commands are developed.
- The `run` command currently waits for process exit and then prints the retained transcript; live terminal bridging is planned.
- JSON-RPC control is planned and will be separate from raw terminal output.

## Development environments

- Use Nix on macOS and Linux when available.
- Use Cargo directly on Windows.
- Keep code paths portable unless a module is explicitly platform-specific.

## Implementation notes

- Unix PTYs and Windows ConPTY sit behind the `Session` abstraction.
- Public APIs use ptywright-owned types rather than backend crate types.
- Path handling should use `Path`/`PathBuf`, not string concatenation.
- Tests should avoid shell-specific assumptions unless they are platform-gated and documented.
- Release artifacts should keep platform naming explicit.
