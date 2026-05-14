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
- JSON-RPC control is planned as a separate `serve --stdio` mode so protocol output never mixes with raw terminal bytes.

## Exit behavior

- Help and version output exit with status `0`.
- `run` exits with the child process status when available.
- Unknown flags are rejected by clap and exit non-zero.
