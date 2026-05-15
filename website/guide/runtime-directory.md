# Runtime directory

ptywright keeps configuration, log files, and any future cached state under a single per-user directory. The default is `~/.ptywright/`, deliberately co-located rather than split across XDG locations so the tree is easy to inspect, back up, and remove on macOS, Linux, and Windows alike.

## Layout

```text
~/.ptywright/
├── config.toml      # optional — falls back to defaults when missing
├── logs/            # daily-rotated log files (ptywright.YYYY-MM-DD.log)
├── data/            # reserved for future on-disk state
├── transcripts/     # reserved for opt-in raw transcript files
└── sockets/         # reserved for IPC paths created by serve --socket
```

ptywright creates each subdirectory lazily, only when the corresponding feature writes to it.

## Resolution order

The runtime root is resolved in this order:

1. The `PTYWRIGHT_HOME` environment variable, if set and non-empty.
2. `~/.ptywright/` (via the platform's home-directory lookup).
3. `./.ptywright` as a last resort if `HOME` is unset.

`PTYWRIGHT_HOME=/some/path` lets you sandbox an entire ptywright invocation (config, logs, sockets) into a temporary directory — useful for tests and per-project setups.

## Config file

`~/.ptywright/config.toml` is optional. When the file is missing or any individual key is absent, the documented default is used. The full set of recognized keys today:

```toml
[logging]
# tracing-subscriber EnvFilter directive. Examples:
#   "warn"                          quiet by default
#   "info"                          general progress
#   "info,ptywright::rpc=debug"     RPC traces, info elsewhere
#   "trace"                         everything
# Override at runtime with the PTYWRIGHT_LOG environment variable.
level = "warn"

# Write rotated log files under <home>/logs/. Disable to keep stderr-only.
file = true

# Maximum age in days for retained log files. 0 disables retention.
max_days = 14

# Output format for log records. "text" or "json".
format = "text"
```

Unknown top-level keys and unknown sections are ignored, so older binaries still parse files written by newer ones.

A copy of this template lives at [`config.example.toml`](https://github.com/utensils/ptywright/blob/main/config.example.toml) in the repo root.

## Logging

ptywright uses [`tracing`](https://docs.rs/tracing/) with daily-rotated files written through a non-blocking background writer. Each CLI mode wires the right combination of sinks so the output contracts of the binary are upheld:

| Mode                                 | Stderr | File | Notes                                                                                           |
| ------------------------------------ | :----: | :--: | ----------------------------------------------------------------------------------------------- |
| `ptywright run`                      |   ✗    |  ✓   | The `run` command bridges raw bytes to your terminal — extra stderr would corrupt the live PTY. |
| `ptywright serve --stdio`            |   ✓    |  ✓   | stdout is reserved for JSON-RPC framing and is never written.                                   |
| `ptywright serve --socket`           |   ✓    |  ✓   | Same as `--stdio`.                                                                              |
| `--help`, `--version`, `completions` |   ✓    |  ✗   | Minimal stderr-only init for short-lived commands.                                              |

### File names and rotation

Files are named `ptywright.YYYY-MM-DD.log` and rotated daily. On startup, ptywright deletes any `ptywright.*` file under `logs/` whose modification time is older than `logging.max_days` days. Setting `max_days = 0` disables this cleanup so files accumulate indefinitely.

### Filtering

The logging filter follows `tracing-subscriber`'s [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax. Resolution order:

1. The `PTYWRIGHT_LOG` environment variable, if set.
2. `[logging] level` from the config file.
3. `"warn"` as a final fallback.

```bash
PTYWRIGHT_LOG="info,ptywright::rpc=debug" ptywright serve --stdio
```

### Redaction

Every record passes through ptywright's built-in `RedactionPolicy` before it is written to stderr or to disk, so common secret patterns (`api_key=...`, `Bearer <token>`, `sk-...`) are masked even if a downstream callsite forgets to scrub them. Caller-supplied literals and regexes registered through the existing redaction machinery extend the same coverage.

### Format

`logging.format = "json"` switches the file sink to one JSON record per line for ingestion into log-aggregation tools. Stderr stays in human-readable text either way.

## Tilde expansion in config values

Any path-shaped string in `config.toml` accepts a leading `~` or `~/` and expands to the user's home directory at load time. When `HOME` is unset the value is returned verbatim.
