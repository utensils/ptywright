# CLI reference

## `ptywright`

Print the help menu.

```bash
ptywright
ptywright --help
```

## `ptywright --version`

Print the binary version.

```bash
ptywright --version
```

Example output:

```text
ptywright 0.1.0
```

## `ptywright run`

Run a command in a headless PTY and bridge stdin/stdout live. This is intended for local debugging and smoke testing; use `serve --stdio` for machine-readable automation.

When attached to an interactive terminal, `run` temporarily enables raw mode and filters terminal-generated focus/capability response sequences so they are not echoed into line-mode prompts. Raw mode is restored on normal exit and panic unwinding; if the process is forcibly killed and your terminal is left in raw mode, run `stty sane` to recover.

```bash
ptywright run -- /bin/sh -lc 'printf ready'
```

Options:

| Option     | Default | Meaning                   |
| ---------- | ------- | ------------------------- |
| `--rows N` | `24`    | Initial terminal rows.    |
| `--cols N` | `80`    | Initial terminal columns. |

Example:

```bash
ptywright run --rows 40 --cols 120 -- /bin/sh -lc 'stty size; printf done'
```

`run` exits with the child process status when available.

## `ptywright serve --stdio`

Start the JSON-RPC 2.0 server on stdin/stdout.

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | ptywright serve --stdio
ptywright serve --stdio --framing lsp
ptywright serve --socket /tmp/ptywright.sock
```

Rules:

- `--stdio` uses stdin/stdout. stdout is protocol-only; stderr is reserved for diagnostics.
- `--socket PATH` listens on a local IPC endpoint: a Unix domain socket on macOS/Linux, or a named pipe on Windows (e.g. `\\.\pipe\ptywright`). Both share the same flag and the same multi-client server state.
- Default framing is `ndjson`: each input line is one complete JSON-RPC request or notification, and each response/notification is one compact JSON object per line.
- `--framing lsp` uses `Content-Length: N\r\n\r\n<json>` frames.

See [JSON-RPC](./json-rpc.md) for methods and payloads.

## `ptywright completions`

Generate shell completion registration scripts.

```bash
ptywright completions bash
ptywright completions zsh
ptywright completions fish
ptywright completions elvish
ptywright completions powershell
```

Setup examples:

```bash
# zsh, add to ~/.zshrc
source <(ptywright completions zsh)

# bash, add to ~/.bashrc
source <(ptywright completions bash)

# fish, persist to completions dir
ptywright completions fish > ~/.config/fish/completions/ptywright.fish
```

Supported shells are `bash`, `zsh`, `fish`, `elvish`, and `powershell`.

## Runtime directory and logging

ptywright keeps configuration, log files, and other per-user state under `~/.ptywright/` (override the root entirely with `PTYWRIGHT_HOME=/some/path`). See the [Runtime directory guide](../guide/runtime-directory.md) for the full layout, config schema, and logging details.

### Per-mode log sinks

| Subcommand                           | Stderr | File | Notes                                                                               |
| ------------------------------------ | :----: | :--: | ----------------------------------------------------------------------------------- |
| `ptywright run`                      |   ✗    |  ✓   | `run` bridges raw bytes to your terminal — extra stderr would corrupt the live PTY. |
| `ptywright serve --stdio`            |   ✓    |  ✓   | stdout is JSON-RPC framing only and is never written.                               |
| `ptywright serve --socket`           |   ✓    |  ✓   | Same sinks as `--stdio`.                                                            |
| `--help`, `--version`, `completions` |   ✓    |  ✗   | Minimal stderr-only init for short-lived commands.                                  |

### Environment variables

| Variable         | Purpose                                                                                       |
| ---------------- | --------------------------------------------------------------------------------------------- |
| `PTYWRIGHT_HOME` | Root for the runtime directory. Overrides the default `~/.ptywright/`.                        |
| `PTYWRIGHT_LOG`  | `tracing-subscriber` `EnvFilter` directive. Overrides `[logging] level` from the config file. |

```bash
PTYWRIGHT_LOG="info,ptywright::rpc=debug" ptywright serve --stdio
PTYWRIGHT_HOME=/tmp/ptywright-sandbox ptywright run -- /bin/sh -lc 'printf hi'
```

### Log files

Files are written to `<PTYWRIGHT_HOME>/logs/ptywright.YYYY-MM-DD.log`, rotated daily, and pruned at startup against `[logging] max_days` (default `14`; set `0` to disable retention). Every record passes through ptywright's built-in [`RedactionPolicy`](./library.md#redaction) before reaching disk or stderr.

## Exit behavior

- Help and version output exit with status `0`.
- `run` exits with the child process status when available.
- `serve --stdio` exits with status `0` when stdin reaches EOF without an unrecoverable I/O error.
- `completions <shell>` exits with status `0` for supported shells and non-zero for unknown shells.
- Unknown flags are rejected by clap and exit non-zero.
