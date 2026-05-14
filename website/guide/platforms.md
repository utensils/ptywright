# Platforms

ptywright is intended to work across macOS, Linux, and Windows.

## Current status

The initial PTY implementation uses `portable-pty`, which maps to Unix PTYs on macOS/Linux and ConPTY on Windows. The crate is built and checked on all three operating systems in CI.

| Platform | Status            | Notes                                                                                                            |
| -------- | ----------------- | ---------------------------------------------------------------------------------------------------------------- |
| macOS    | Initial PTY path  | CI runs check, clippy, tests, and release build.                                                                 |
| Linux    | Initial PTY path  | CI runs check, clippy, tests, and release build.                                                                 |
| Windows  | Compile validated | CI builds/checks the PTY code. Use `serve --stdio` for automation transport while named-pipe parity is designed. |

## Current limitations

- Unix PTY behavior has deterministic tests for simple command output.
- Windows ConPTY support is compiled in, but command-output fixtures are temporarily gated while deterministic Windows PTY test commands are developed.
- `ptywright run` bridges stdin/stdout live for local debugging.
- JSON-RPC control is available through `serve --stdio` on all platforms and Unix sockets on macOS/Linux.
- Windows named-pipe server mode is not implemented yet; `serve --stdio` is the documented Windows equivalent for local automation transport for now.

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
