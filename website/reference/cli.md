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
ptywright 0.2.0
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

## `ptywright repl`

Interactive REPL client for a running `ptywright serve`. Shipped as the default-on `repl` Cargo feature — pass `--no-default-features` at build time to opt out of the `reedline` / `crossbeam-channel` / `nu-ansi-term` dependencies.

```bash
# Connect to a running daemon (Unix domain socket or Windows named pipe).
ptywright serve --socket /tmp/ptywright.sock &
ptywright repl --socket /tmp/ptywright.sock

# …or spawn a child server and pipe JSON-RPC over its stdio in one command.
ptywright repl --stdio -- ptywright serve --stdio
```

Options:

| Option              | Default  | Meaning                                                                |
| ------------------- | -------- | ---------------------------------------------------------------------- |
| `--socket PATH`     | —        | Connect to a server listening on `PATH`.                               |
| `--stdio`           | —        | Spawn a child server (pass its command + args after `--`).             |
| `--framing FRAMING` | `ndjson` | JSON-RPC framing (`ndjson` or `lsp`). Must match the server's framing. |

If neither `--socket` nor `--stdio` is supplied, the REPL targets the default socket at `~/.ptywright/socket`. When a server is already listening there it connects directly. When nothing is listening, the REPL **auto-spawns** a background `ptywright serve --socket ~/.ptywright/socket` (the same binary, detached stdio, its own process group) and connects once the socket is accepting. On exit you are prompted whether to shut that auto-started server down (the default) or detach it so other clients can keep using it; a non-interactive stdin defaults to shutdown so scripted invocations never leak a daemon. Passing `--socket <path>` explicitly skips the auto-spawn — that form assumes you are managing the server yourself.

The REPL is a sequential `reedline`-based loop and **each input line is real Lua 5.4** evaluated against a curated set of REPL-bound globals. Line editing, completion, syntax highlighting, history, multi-line continuation, and ghost-text hinting are delegated to reedline; the bindings drive the generic `adapter.*` JSON-RPC surface. The most common forms:

Lua's call-with-table-argument sugar (`f{...}`) and call-with-string sugar (`f"..."`) make the REPL surface read naturally:

```lua
plugins()                                -- list built-in plugins
plugins.describe "claude-code"            -- string sugar — same as plugins.describe("claude-code")

session.spawn{ "claude-code", rows = 24, cols = 80 }   -- combined form
session.spawn{ "claude-code", args = {"--model", "haiku"}, env = { NO_COLOR = "1" } }
session.spawn("claude-code", { rows = 24 })            -- equivalent parenthesised form
session.resume{ "claude-code", prior_adapter = "e1", args = {"--resume", "abc"} }
session.list()                           -- local tabs in this REPL
session.live()                           -- all adapters live on the server
session.attach "e3"                       -- adopt a sibling connection's adapter
session.attach "all"                      -- adopt every live adapter at once

send.text "hello"                         -- send a prompt
send.key "shift-tab"                      -- send a single named key
send.intent{ "approve", reason = "ok" }   -- invoke a plugin intent with kwargs

wait(matches "^❯")                        -- wait for a regex match
wait(screen_stable(250))                  -- wait for the screen to settle (250 ms)
wait.matches "^❯"                         -- shorthand with the default matcher
wait.screen_stable(s(2))                  -- s(n) is sugar for n * 1000 ms
turn{ "send_prompt", prompt = "go", wait = matches "done", timeout = s(5) }

state()                                  -- re-classify the focused adapter
screen.snapshot() / view()                -- render the PTY inline (styled)
transcript.snapshot{ redact = false }     -- dump the transcript
inspect()                                -- diagnostic adapter dump
```

Because each line is real Lua, you also have the full standard library
and language available — variables, loops, conditionals, `os.date()`,
`string.format(...)`, etc:

```lua
for i = 1, 3 do session.spawn("claude-code") end
local replies = {}
for _, adapter in ipairs(session.list()) do
  -- focus, fetch, store …
end
```

Multi-line input is supported: any unclosed block / table / function
opens a continuation prompt (`…`) and Enter on the closing `end` /
`}` submits the whole chunk.

### Globals reference

| Global | Form | Notes |
| --- | --- | --- |
| `plugins` | `plugins()` / `plugins.describe("name")` | Read-only registry queries. |
| `session.spawn` | `(plugin, opts?)` or `{plugin, ...opts}` | Optional opts: `program`, `args`, `cwd`, `env`, `rows`, `cols`, `pixel_width`, `pixel_height`. |
| `session.resume` | same shape + `prior_adapter` / `prior` | Closes the prior adapter if still live. |
| `session.list` / `session.live` / `session.attach` / `session.close` | — | Local tab list / server-side list / adopt / close. |
| `state` | `state()` | `adapter.state` for the focused adapter. |
| `send.text` / `send.key` / `send.intent` | `(string)` or `(intent, opts?)` | `send.text` wires to `intent=send_prompt` with `{prompt}`. |
| `turn` | `(intent, opts?)` | Reserved opts: `wait` (a matcher), `wait_intent` (override the matcher fn), `timeout` (ms). |
| `wait` | `wait(matcher, opts?)` / `wait.matches("...", opts?)` / `wait.screen_stable(ms, opts?)` | `wait_id = "..."` makes the wait cancellable from another connection. |
| `matches` / `screen_stable` | `matches("regex")` / `screen_stable(ms)` | Return tagged matcher values; standalone calls do no RPC. |
| `cancel_wait` | `cancel_wait("wait-id")` | Break a still-in-flight wait. |
| `transcript.snapshot` | `({ redact = true })` | `redact` defaults to true. |
| `screen.snapshot` / `view` | `()` | Renders the focused PTY inline; `view()` is an alias. |
| `inspect` | `inspect()` | Adapter diagnostic dump. |
| `re(s)` / `ms(n)` / `s(n)` | — | `re` and `ms` are identity (document intent); `s(n)` returns `n * 1000`. |

`send.key(...)` accepts the full host `Key` surface (see the [Lua extension API](../guide/extensions.md#host-api-exposed-to-lua-plugins)) with hyphens as a convenience: `enter`, `escape`, `tab`, `shift-tab`, `backspace`, `delete`, `space`, the arrows, the navigation cluster (`home`, `end`, `page-up`, `page-down`, `insert`), every `ctrl-a` through `ctrl-z` except the four that alias named keys (`ctrl-h`/`ctrl-i`/`ctrl-j`/`ctrl-m`), and `f1` through `f12`. Single characters that aren't aliases (`"y"`, `"n"`, `"1"`) fall through to typed text so quick acknowledgements work without dropping to `send.text`. Tab completion lists the most common keys (submit/cancel/edit, arrows, navigation) first.

Meta commands prefixed with `:` cover REPL control and a raw JSON-RPC escape hatch:

| Command                  | Effect                                                                                                                                                                      |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `:tabs`                  | List local adapter tabs.                                                                                                                                                    |
| `:focus <id>`            | Switch focus to an adapter id.                                                                                                                                              |
| `:live`                  | List adapters live on the server (cross-connection visibility).                                                                                                             |
| `:attach <id\|all>`      | Adopt a sibling connection's adapter (tmux-style attach). `all` adopts every live adapter; `:attach <id>` auto-renders the adapter's current screen on attach.              |
| `:notifications on\|off` | Subscribe / unsubscribe to `session.changed` / `session.exited` events. Notifications are enabled by default and rendered above the prompt via reedline's external printer. |
| `:rpc <method> {json}`   | Send a raw JSON-RPC call and dump the response.                                                                                                                             |
| `:tips`                  | Show a curated five-section tour of the most useful idioms.                                                                                                                 |
| `:help`                  | Show the inline help popup.                                                                                                                                                 |
| `:quit`                  | Exit the REPL (also `Ctrl-D` on an empty prompt).                                                                                                                           |

On launch the banner surfaces one rotating one-liner tip (a different one most sessions) plus a pointer to `:tips` and `:help`. History is persisted to `~/.ptywright/repl-history` so previous sessions remain reachable through `Ctrl-R` reverse-search.

A 500 ms `server.capabilities` heartbeat keeps the server's per-connection notification pump warm so events from sibling connections actually flush to an idle REPL. The heartbeat call is read-only by design, so it cannot race with a concurrent `:notifications off`.

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

## `ptywright logs`

Tail the most recent ptywright log file under `<PTYWRIGHT_HOME>/logs/`. Reads the newest file matching `ptywright.*` (the daily-rotation pattern), prints the last `--lines` lines (default 20, matching `tail -n 20 -f` convention), and follows the file for new content until Ctrl-C.

```bash
ptywright logs                          # last 20 lines, then follow
ptywright logs --lines 200              # backfill 200 lines before following
ptywright logs --filter rpc             # only lines containing "rpc"
ptywright logs --filter PermissionDenied
```

Behaviour notes:

- The subcommand never writes to the log file itself; it's a passive reader.
- An empty `<PTYWRIGHT_HOME>/logs/` exits non-zero with a clear "no ptywright log files" message — run any other subcommand once first to seed the directory.
- Filtering is substring-based, not the `tracing-subscriber` `EnvFilter` grammar — that filter is set at server startup via `PTYWRIGHT_LOG`, not at tail time.
- Poll cadence is 200 ms. Not real-time enough for sub-second debugging; use `tail -F` directly if you need wire-speed.

## Runtime directory and logging

ptywright keeps configuration, log files, and other per-user state under `~/.ptywright/` (override the root entirely with `PTYWRIGHT_HOME=/some/path`). See the [Runtime directory guide](../guide/runtime-directory.md) for the full layout, config schema, and logging details.

### Per-mode log sinks

| Subcommand                                   | Stderr | File | Notes                                                                                                                                                                                                                           |
| -------------------------------------------- | :----: | :--: | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ptywright run`                              |   ✗    |  ✓   | `run` bridges raw bytes to your terminal — extra stderr would corrupt the live PTY.                                                                                                                                             |
| `ptywright serve --stdio`                    |   ✓    |  ✓   | stdout is JSON-RPC framing only and is never written.                                                                                                                                                                           |
| `ptywright serve --socket`                   |   ✓    |  ✓   | Same sinks as `--stdio`.                                                                                                                                                                                                        |
| `ptywright repl`                             |   ✓    |  ✓   | Uses the oneshot init. The REPL is a sequential reedline loop that writes its prompt to stdout interleaved with the operator's commands; stderr is rare in normal use, but any messages that do land share the same scrollback. |
| `--help`, `--version`, `completions`, `logs` |   ✓    |  ✗   | Minimal stderr-only init for short-lived commands. `logs` reads existing files without writing new entries.                                                                                                                     |

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
