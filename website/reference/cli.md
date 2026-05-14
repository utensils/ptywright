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

Run a command in a headless PTY and print its captured transcript after the command exits.

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

Current limitations:

- `run` waits for the child to exit before printing the retained transcript.
- Live stdin/stdout bridging is planned.

## `ptywright serve --stdio`

Start the NDJSON-framed JSON-RPC 2.0 server on stdin/stdout.

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | ptywright serve --stdio
```

Rules:

- stdout is protocol-only.
- stderr is reserved for diagnostics.
- Each input line is one complete JSON-RPC request or notification.
- Responses are one compact JSON object per line.

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

## Exit behavior

- Help and version output exit with status `0`.
- `run` exits with the child process status when available.
- `serve --stdio` exits with status `0` when stdin reaches EOF without an unrecoverable I/O error.
- `completions <shell>` exits with status `0` for supported shells and non-zero for unknown shells.
- Unknown flags are rejected by clap and exit non-zero.
