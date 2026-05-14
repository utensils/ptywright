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

## Exit behavior

- Help and version output exit with status `0`.
- Unknown flags are rejected by clap and exit non-zero.
