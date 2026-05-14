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

## Exit behavior

- Help and version output exit with status `0`.
- `run` exits with the child process status when available.
- `serve --stdio` exits with status `0` when stdin reaches EOF without an unrecoverable I/O error.
- Unknown flags are rejected by clap and exit non-zero.
